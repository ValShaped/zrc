//! Code generation for switch statements

use inkwell::{basic_block::BasicBlock, debug_info::DILexicalBlock};
use zrc_typeck::tast::{
    expr::TypedExpr,
    stmt::{LetDeclaration, TypedStmt},
};
use zrc_utils::span::Spanned;

use crate::{
    bb::BasicBlockAnd,
    ctx::{BlockCtx, FunctionCtx},
    expr::cg_expr,
    scope::CgScope,
    stmt::{LoopBreakaway, cg_block},
    unpack,
};

/// Code generates a for statement
#[allow(clippy::too_many_arguments, clippy::ref_option, clippy::box_collection)]
pub fn cg_for_stmt<'ctx, 'input, 'a>(
    cg: FunctionCtx<'ctx, 'a>,
    bb: BasicBlock<'ctx>,
    scope: &'a CgScope<'input, 'ctx>,
    lexical_block: DILexicalBlock<'ctx>,
    init: Option<Box<Vec<Spanned<LetDeclaration<'input>>>>>,
    cond: Option<TypedExpr<'input>>,
    post: Option<TypedExpr<'input>>,
    body: Spanned<Vec<TypedStmt<'input>>>,
) -> BasicBlock<'ctx> {
    // For loops generate a somewhat more complicated CFG, with a few parts.
    // The preheader, where `init` runs. Breaks to the header.
    // The header, where `cond` is checked and breaks to either the exit or the
    // body. The body, where most of the body runs. Breaks to
    // the latch. `break` transfers to the exit by force and `continue` transfers to
    // the latch by force. The latch, where `post` runs and
    // breaks back to the header The exit, which is the basic
    // block we return.

    // loops lie in an implicit subscope
    let mut scope = scope.clone();

    // The block we are currently in will become the preheader. Generate the `init`
    // code if there is any.
    if let Some(init) = init {
        super::let_decl::cg_let_declaration(cg, bb, &mut scope, lexical_block, *init);
    }

    let expr_cg = BlockCtx::new(cg, &scope, lexical_block);

    let header = cg.ctx.append_basic_block(cg.fn_value, "header");
    let body_bb = cg.ctx.append_basic_block(cg.fn_value, "body");
    let latch = cg.ctx.append_basic_block(cg.fn_value, "latch");
    let exit = cg.ctx.append_basic_block(cg.fn_value, "exit");

    // Branch to the header from the preheader.
    cg.builder
        .build_unconditional_branch(header)
        .expect("branch should generate successfully");

    // Generate the header.
    cg.builder.position_at_end(header);
    let header = cond.map_or_else(
        || {
            // If there is no condition, we always branch to the body.
            cg.builder
                .build_unconditional_branch(body_bb)
                .expect("branch should generate successfully");

            header
        },
        |cond| {
            let mut header = header;

            let cond = unpack!(header = cg_expr(expr_cg, header, cond));

            cg.builder
                .build_conditional_branch(cond.into_int_value(), body_bb, exit)
                .expect("branch should generate successfully");

            header
        },
    );

    // Generate the body.
    cg.builder.position_at_end(body_bb);
    let body_bb = cg_block(
        cg,
        body_bb,
        &scope,
        lexical_block,
        body,
        &Some(LoopBreakaway {
            on_break: exit,
            on_continue: latch,
        }),
    );

    // The body breaks to latch
    if body_bb.is_some() {
        cg.builder
            .build_unconditional_branch(latch)
            .expect("branch should generate successfully");
    }

    // Latch runs post and then breaks right back to the header.
    cg.builder.position_at_end(latch);
    if let Some(post) = post {
        cg_expr(expr_cg, latch, post);
    }

    cg.builder
        .build_unconditional_branch(header)
        .expect("branch should generate successfully");

    cg.builder.position_at_end(exit);

    exit
}

/// Code generates a while statement
#[allow(clippy::too_many_arguments, clippy::ref_option)]
pub fn cg_while_stmt<'ctx, 'input, 'a>(
    cg: FunctionCtx<'ctx, 'a>,
    scope: &'a CgScope<'input, 'ctx>,
    lexical_block: DILexicalBlock<'ctx>,
    cond: TypedExpr<'input>,
    body: Spanned<Vec<TypedStmt<'input>>>,
) -> BasicBlock<'ctx> {
    let expr_cg = BlockCtx::new(cg, scope, lexical_block);

    // While loops are similar to for loops but much simpler.
    // The preheader simply just breaks to the header.
    // The header checks the condition and breaks to the exit or the body.
    // The body simply breaks to the header.
    // The exit is the continued code

    // `break` => exit
    // `continue` => header

    let mut header = cg.ctx.append_basic_block(cg.fn_value, "header");

    let body_bb = cg.ctx.append_basic_block(cg.fn_value, "body");

    let exit = cg.ctx.append_basic_block(cg.fn_value, "exit");

    cg.builder
        .build_unconditional_branch(header)
        .expect("branch should generate successfully");

    cg.builder.position_at_end(header);

    let cond = unpack!(header = cg_expr(expr_cg, header, cond));

    cg.builder
        .build_conditional_branch(cond.into_int_value(), body_bb, exit)
        .expect("branch should generate successfully");

    cg.builder.position_at_end(body_bb);

    let body_bb = cg_block(
        cg,
        body_bb,
        scope,
        lexical_block,
        body,
        &Some(LoopBreakaway {
            on_break: exit,
            on_continue: header,
        }),
    );

    if body_bb.is_some() {
        cg.builder
            .build_unconditional_branch(header)
            .expect("branch should generate successfully");
    }

    cg.builder.position_at_end(exit);
    exit
}

/// Code generates a do..while statement
#[allow(clippy::too_many_arguments, clippy::ref_option)]
pub fn cg_do_while_stmt<'ctx, 'input, 'a>(
    cg: FunctionCtx<'ctx, 'a>,
    scope: &'a CgScope<'input, 'ctx>,
    lexical_block: DILexicalBlock<'ctx>,
    body: Spanned<Vec<TypedStmt<'input>>>,
    cond: TypedExpr<'input>,
) -> BasicBlock<'ctx> {
    let expr_cg = BlockCtx::new(cg, scope, lexical_block);

    // `do..while` loops are slightly different from `while` loops.
    // the preheader breaks directly to the *body* and forces it to run at
    // least once. the body can then later break to the header which checks the
    // condition and will loop or exit.

    // `break` => exit
    // `continue` => header

    let body_bb = cg.ctx.append_basic_block(cg.fn_value, "body");
    let body_start = body_bb;

    let header = cg.ctx.append_basic_block(cg.fn_value, "header");

    let exit = cg.ctx.append_basic_block(cg.fn_value, "exit");

    cg.builder
        .build_unconditional_branch(body_bb)
        .expect("branch should generate successfully");

    cg.builder.position_at_end(body_bb);

    let body_bb = cg_block(
        cg,
        body_bb,
        scope,
        lexical_block,
        body,
        &Some(LoopBreakaway {
            on_break: exit,
            on_continue: header,
        }),
    );

    if body_bb.is_some() {
        cg.builder
            .build_unconditional_branch(header)
            .expect("branch should generate successfully");
    }

    cg.builder.position_at_end(header);

    let BasicBlockAnd { value: cond, .. } = cg_expr(expr_cg, header, cond);

    cg.builder
        .build_conditional_branch(cond.into_int_value(), body_start, exit)
        .expect("branch should generate successfully");

    cg.builder.position_at_end(exit);

    exit
}

#[cfg(test)]
mod tests {
    // Please read the "Common patterns in tests" section of crate::test_utils for
    // more information on how code generator tests are structured.

    use indoc::indoc;

    use crate::cg_snapshot_test;

    #[test]
    fn unreachable_statement_generates_properly() {
        cg_snapshot_test!(indoc! {"
            fn test(cond: bool) {
                let x = 7;
                if (x == 6) unreachable;
                else return;
            }
        "});
    }

    #[test]
    fn while_loops_along_with_break_and_continue_generate_as_expected() {
        cg_snapshot_test!(indoc! {"
                    fn get_bool() -> bool;

                    fn test() {
                        // TEST: the proper while loop structure is created
                        while (get_bool()) {
                            // TEST: break jumps to the `end` block
                            if (get_bool()) break;
                            else {
                                // TEST: continue jumps to the header block
                                if (get_bool()) continue;
                                // TEST: otherwise, we proceed
                                else {}
                            }

                            // TEST: the loop jumps back to the header block
                        }

                        // TEST: ...and code generation properly continues.
                        return;
                    }
                "});
    }

    #[test]
    fn for_loops_along_with_break_and_continue_generate_as_expected() {
        cg_snapshot_test!(indoc! {"
                    fn get_int() -> i32;

                    fn test() {
                        // TEST: the proper while loop structure is created
                        for (let i = 0; i < get_int(); i += 1) {
                            // TEST: break jumps to the `end` block
                            if (i > get_int()) break;
                            else {
                                // TEST: continue jumps to the latch block
                                if (i < get_int()) continue;
                                else {}
                            }

                            // TEST: the loop jumps to the latch block which jumps back to the
                            // header
                        }
                    }
                "});
    }

    #[test]
    fn do_while_loops_generate_as_expected() {
        cg_snapshot_test!(indoc! {"
                    fn get_bool() -> bool;

                    fn test() {
                        // TEST: the proper `do..while` loop structure is created
                        do {
                            get_bool(); // for fake side effects
                        } while (get_bool());
                    }
                "});
    }

    #[test]
    fn switch_statements_generate_as_expected() {
        cg_snapshot_test!(indoc! {"
                    fn get_bool() -> bool;
                    fn when_true();
                    fn when_false_a();
                    fn when_false_b();
                    fn when_default(x: i32);
                    fn post();


                    fn test() {
                        // TEST: the proper `switch` structure is created
                        switch (get_bool()) {
                            true => when_true();
                            false => {
                                when_false_a();
                                when_false_b();
                            }
                            default => {
                                let x = 2 + 2;
                                when_default(x);
                            }
                        }
                        post();
                    }
                "});
    }
}
