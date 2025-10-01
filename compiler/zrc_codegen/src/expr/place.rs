//! Code generation for Place types

use inkwell::{
    basic_block::BasicBlock,
    debug_info::AsDIScope,
    values::{BasicValue, PointerValue},
};
use zrc_typeck::tast::{
    expr::{Place, PlaceKind},
    ty::Type,
};

use super::cg_expr;
use crate::{
    bb::{BasicBlockAnd, BasicBlockExt},
    ctx::BlockCtx,
    ty::llvm_basic_type,
    unpack,
};

/// Resolve a place to its pointer
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn cg_place<'ctx>(
    cg: BlockCtx<'ctx, '_, '_>,
    mut bb: BasicBlock<'ctx>,
    place: Place,
) -> BasicBlockAnd<'ctx, PointerValue<'ctx>> {
    let place_span = place.kind.span();
    let line_and_col = cg.line_lookup.lookup_from_index(place_span.start());
    let debug_location = cg.dbg_builder.create_debug_location(
        cg.ctx,
        line_and_col.line,
        line_and_col.col,
        cg.dbg_scope.as_debug_info_scope(),
        None,
    );
    cg.builder.set_current_debug_location(debug_location);

    match place.kind.into_value() {
        PlaceKind::Variable(x) => {
            let reg = cg
                .scope
                .get(x)
                .expect("identifier that passed typeck should exist in the CgScope");

            bb.and(reg)
        }

        PlaceKind::Deref(x) => {
            let value = unpack!(bb = cg_expr(cg, bb, *x));

            bb.and(value.into_pointer_value())
        }

        PlaceKind::Index(ptr, idx) => {
            let ptr = unpack!(bb = cg_expr(cg, bb, *ptr));
            let idx = unpack!(bb = cg_expr(cg, bb, *idx));

            // SAFETY: If indices are used incorrectly this may segfault
            // TODO: Is this actually safely used?
            let reg = unsafe {
                cg.builder.build_gep(
                    llvm_basic_type(&cg, &place.inferred_type).0,
                    ptr.into_pointer_value(),
                    &[idx.into_int_value()],
                    "gep",
                )
            }
            .expect("building GEP instruction should succeed");

            bb.and(reg.as_basic_value_enum().into_pointer_value())
        }

        #[allow(clippy::wildcard_enum_match_arm)]
        PlaceKind::Dot(x, prop) => match &x.inferred_type {
            Type::Struct(contents) => {
                let x_ty = llvm_basic_type(&cg, &x.inferred_type).0;
                let prop_idx = contents
                    .iter()
                    .position(|(got_key, _)| *got_key == prop.into_value())
                    .expect("invalid struct field");

                let x = unpack!(bb = cg_place(cg, bb, *x));

                let reg = cg
                    .builder
                    .build_struct_gep(
                        x_ty,
                        x,
                        prop_idx
                            .try_into()
                            .expect("got more than u32::MAX as key index? HOW?"),
                        "gep",
                    )
                    .expect("building GEP instruction should succeed");

                bb.and(reg.as_basic_value_enum().into_pointer_value())
            }
            Type::Union(_) => {
                // All we need to do is cast the pointer, but there's no `bitcast` anymore,
                // so just return it and it'll take on the correct type

                let value = unpack!(bb = cg_place(cg, bb, *x));

                bb.and(value)
            }
            _ => panic!("cannot access property of non-struct"),
        },
    }
}

#[cfg(test)]
mod tests {
    // Please read the "Common patterns in tests" section of crate::test_utils for
    // more information on how code generator tests are structured.

    use indoc::indoc;

    use crate::cg_snapshot_test;

    // Remember: In all of these tests, cg_place returns a *pointer* to the data in
    // the place.

    #[test]
    fn basic_identifiers_in_place_position() {
        cg_snapshot_test!(indoc! {"
                fn test() {
                    let x = 6;

                    // TEST: we should simply be `store`ing to the %let_x we created
                    x = 7;
                }
            "});
    }

    #[test]
    fn identifier_deref_generates_as_expected() {
        cg_snapshot_test!(indoc! {"
                fn test() {
                    let x: *i32;

                    // TEST: x is *i32, so %let_x is **i32 (ptr to the stack). we should load from
                    // %let_x to obtain the actual pointer. we should then store to that result
                    // value (we should never load it)
                    *x = 4;
                }
            "});
    }

    #[test]
    fn other_deref_generates_as_expected() {
        cg_snapshot_test!(indoc! {"
                fn test() {
                    // TEST: because cg_place returns a *pointer* to the represented value, handling
                    // *5 in a place context should return the address of *5, which is &*5 = 5.
                    // for this reason, we should literally be `store`ing to the hardcoded address
                    // 5, and never *loading* from it (because if we do load we may not be actually
                    // writing to that address)
                    // we use 5 not 0 because 0 is just 'ptr null'
                    *(5 as *i32) = 0;
                }
            "});
    }

    #[test]
    fn pointer_indexing_in_place_position() {
        cg_snapshot_test!(indoc! {"
                fn test() {
                    let x: *i32;

                    // TEST: `x` is *i32, so %let_x is a **i32 (ptr to the stack).
                    // %let_x needs to be GEP'd into and then stored into, but we must not load
                    // from the address.
                    x[4 as usize] = 5;
                }
            "});
    }

    #[test]
    fn struct_property_access_in_place_position() {
        cg_snapshot_test!(indoc! {"
                struct S { x: i32, y: i32 }

                fn test() {
                    let x: S;

                    // TEST: the value must NOT be loaded! it must simply gep to obtain a pointer,
                    // then `store` into that pointer.
                    x.y = 4;
                }
            "});
    }

    #[test]
    fn union_property_access_in_place_position() {
        cg_snapshot_test!(indoc! {"
                union U { x: i32, y: i8 }

                fn test() {
                    let x: U;

                    // TEST: the pointer is cast and then written to as an i32
                    x.x = 4;

                    // TEST: the pointer is cast and then written to as an i8
                    x.y = 5 as i8;
                }
            "});
    }
}
