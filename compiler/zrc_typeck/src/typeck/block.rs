//! for blocks

mod block_utils;
mod branch;
mod cfa;
mod loops;
mod switch_match;

pub use block_utils::{coerce_stmt_into_block, has_duplicates};
pub use cfa::{BlockReturnAbility, BlockReturnActuality};
use zrc_diagnostics::{Diagnostic, DiagnosticKind, Severity};
use zrc_parser::ast::stmt::{Stmt, StmtKind};
use zrc_utils::span::{Span, Spannable, Spanned};

use super::{declaration::process_let_declaration, expr::try_coerce_to, scope::Scope, type_expr};
use crate::tast::{
    stmt::{TypedStmt, TypedStmtKind},
    ty::Type as TastType,
};

/// Type check a block of [AST statement](Stmt)s and return a block of [TAST
/// statement](TypedStmt)s.
///
/// It performs a small desugaring where all statements become implicit blocks.
///
/// This function must be provided a block of statements, and a few bits of
/// information about the parent scope in form of booleans that toggle certain
/// statements like `break` and a [`BlockReturnAbility`].
///
/// # Behavior of block returns
/// In many cases, a block [MUST return](BlockReturnAbility::MustReturn). For
/// example, this is done in the main block of a function. When a function
/// contains sub-blocks, those blocks [*may*
/// return](BlockReturnAbility::MayReturn) but are not required to. However, at
/// least one of the blocks within must be guaranteed to return in order to
/// fulfill a MUST return, otherwise the function is not guaranteed to return.
/// So, if you pass this function a **may** return order, it will return a
/// [`BlockReturnActuality`] which can be used to determine if a MUST return is
/// fulfilled.
///
/// ```rs
/// { // This block must return.
///     { // This block MAY return.
///         if (x) return; // This MAY return.
///     } // This block WILL SOMETIMES return.
///     // Because the above block is not GUARANTEED to return, the "must
///     // return" is not yet satisfied.
/// }
/// ```
///
/// # Errors
/// Errors if a type checker error is encountered.
///
/// # Panics
/// Panics in some internal state failures.
// TODO: Maybe the TAST should attach the BlockReturnActuality in each BlockStmt itself and preserve
// it on sub-blocks in the TAST (this may be helpful in control flow analysis)
#[allow(clippy::too_many_lines)]
pub fn type_block<'input, 'gs>(
    parent_scope: &Scope<'input, 'gs>,
    input_block: Spanned<Vec<Stmt<'input>>>,
    can_use_break_continue: bool,
    return_ability: BlockReturnAbility<'input>,
) -> Result<(Vec<TypedStmt<'input>>, BlockReturnActuality), Diagnostic> {
    let mut scope: Scope<'input, 'gs> = parent_scope.clone();

    let input_block_span = input_block.span();

    // At first, the block does not return.
    let (mut tast_block, return_actualities): (Vec<_>, Vec<_>) = input_block
        .into_value()
        .into_iter()
        .filter_map(
            |stmt| -> Option<Result<(TypedStmt<'input>, BlockReturnActuality), Diagnostic>> {
                let stmt_span = stmt.0.span();
                let inner_closure =
                    || -> Result<Option<(TypedStmt<'_>, BlockReturnActuality)>, Diagnostic> {
                        match stmt.0.into_value() {
                            StmtKind::EmptyStmt => Ok(None),
                            StmtKind::BreakStmt if can_use_break_continue => Ok(Some((
                                TypedStmt(TypedStmtKind::BreakStmt.in_span(stmt_span)),
                                BlockReturnActuality::NeverReturns,
                            ))),
                            StmtKind::BreakStmt => {
                                Err(DiagnosticKind::CannotUseBreakOutsideOfLoop.error_in(stmt_span))
                            }

                            StmtKind::ContinueStmt if can_use_break_continue => Ok(Some((
                                TypedStmt(TypedStmtKind::ContinueStmt.in_span(stmt_span)),
                                BlockReturnActuality::NeverReturns,
                            ))),
                            StmtKind::ContinueStmt => {
                                Err(DiagnosticKind::CannotUseContinueOutsideOfLoop
                                    .error_in(stmt_span))
                            }

                            StmtKind::SwitchCase { scrutinee, cases } => {
                                switch_match::type_switch_case(
                                    &scope,
                                    scrutinee,
                                    &cases,
                                    &return_ability,
                                    stmt_span,
                                )
                            }

                            StmtKind::Match { scrutinee, cases } => switch_match::type_match(
                                &scope,
                                scrutinee,
                                cases,
                                can_use_break_continue,
                                &return_ability,
                                stmt_span,
                            ),

                            StmtKind::UnreachableStmt => Ok(Some((
                                TypedStmt(TypedStmtKind::UnreachableStmt.in_span(stmt_span)),
                                // this may create some weird UB if used incorrectly, but it's on
                                // the user to ensure they don't do that
                                BlockReturnActuality::AlwaysReturns,
                            ))),

                            StmtKind::DeclarationList(declarations) => Ok(Some((
                                TypedStmt(
                                    TypedStmtKind::DeclarationList(process_let_declaration(
                                        &mut scope,
                                        declarations.clone().into_value(),
                                    )?)
                                    .in_span(stmt_span),
                                ),
                                // because expressions can't return...
                                BlockReturnActuality::NeverReturns,
                            ))),

                            StmtKind::IfStmt(cond, then, then_else) => branch::type_if(
                                &scope,
                                cond,
                                then,
                                then_else,
                                can_use_break_continue,
                                &return_ability,
                                stmt_span,
                            ),

                            StmtKind::WhileStmt(cond, body) => {
                                loops::type_while(&scope, cond, body, &return_ability, stmt_span)
                            }
                            StmtKind::DoWhileStmt(body, cond) => {
                                loops::type_do_while(&scope, body, cond, &return_ability, stmt_span)
                            }
                            StmtKind::ForStmt {
                                init,
                                cond,
                                post,
                                body,
                            } => loops::type_for(
                                &scope,
                                init,
                                cond,
                                post,
                                body,
                                &return_ability,
                                stmt_span,
                            ),

                            StmtKind::BlockStmt(body) => {
                                let (typed_body, return_actuality) = type_block(
                                    &scope,
                                    body.in_span(stmt_span),
                                    can_use_break_continue,
                                    return_ability.clone().demote(),
                                )?;
                                Ok(Some((
                                    TypedStmt(
                                        TypedStmtKind::BlockStmt(typed_body).in_span(stmt_span),
                                    ),
                                    return_actuality,
                                )))
                            }

                            StmtKind::ExprStmt(expr) => Ok(Some((
                                TypedStmt(
                                    TypedStmtKind::ExprStmt(type_expr(&scope, expr)?)
                                        .in_span(stmt_span),
                                ),
                                BlockReturnActuality::NeverReturns,
                            ))),
                            StmtKind::ReturnStmt(value) => {
                                let resolved_value =
                                    value.map(|expr| type_expr(&scope, expr)).transpose()?;

                                let inferred_return_type = resolved_value
                                    .clone()
                                    .map_or_else(TastType::unit, |x| x.inferred_type);

                                match (resolved_value, &return_ability) {
                                    // expects no return
                                    (_, BlockReturnAbility::MustNotReturn) => {
                                        Err(DiagnosticKind::CannotReturnHere.error_in(stmt_span))
                                    }

                                    // return x; in fn expecting to return x
                                    (
                                        return_value,
                                        BlockReturnAbility::MustReturn(return_ty)
                                        | BlockReturnAbility::MayReturn(return_ty),
                                    ) => {
                                        let coerced_value = if inferred_return_type == *return_ty {
                                            return_value
                                        } else if inferred_return_type
                                            .can_implicitly_cast_to(return_ty)
                                        {
                                            // Try to coerce the return value to the expected type
                                            return_value.map(|val| try_coerce_to(val, return_ty))
                                        } else {
                                            return Err(Diagnostic(
                                                Severity::Error,
                                                stmt_span.containing(DiagnosticKind::ExpectedGot {
                                                    expected: return_ty.to_string(),
                                                    got: inferred_return_type.to_string(),
                                                }),
                                            ));
                                        };

                                        Ok(Some((
                                            TypedStmt(
                                                TypedStmtKind::ReturnStmt(coerced_value)
                                                    .in_span(stmt_span),
                                            ),
                                            BlockReturnActuality::AlwaysReturns,
                                        )))
                                    }
                                }
                            }
                        }
                    };

                inner_closure().transpose()
            },
        )
        .collect::<Result<Vec<_>, Diagnostic>>()?
        .into_iter()
        .unzip();

    let might_return = return_actualities.iter().any(|x| {
        matches!(
            x,
            BlockReturnActuality::SometimesReturns | BlockReturnActuality::AlwaysReturns
        )
    });
    let will_return = return_actualities
        .iter()
        .any(|x| matches!(x, BlockReturnActuality::AlwaysReturns));

    let return_actuality = match (might_return, will_return) {
        (_, true) => BlockReturnActuality::AlwaysReturns,
        (true, false) => BlockReturnActuality::SometimesReturns,
        (false, false) => BlockReturnActuality::NeverReturns,
    };

    match (return_ability, return_actuality) {
        (
            BlockReturnAbility::MustNotReturn | BlockReturnAbility::MayReturn(_),
            BlockReturnActuality::NeverReturns,
        ) => Ok(BlockReturnActuality::NeverReturns),

        (BlockReturnAbility::MayReturn(_), BlockReturnActuality::SometimesReturns) => {
            Ok(BlockReturnActuality::SometimesReturns)
        }

        (
            BlockReturnAbility::MustReturn(_) | BlockReturnAbility::MayReturn(_),
            BlockReturnActuality::AlwaysReturns,
        ) => Ok(BlockReturnActuality::AlwaysReturns),

        // implicitly add a `return;`
        (
            BlockReturnAbility::MustReturn(return_ty),
            BlockReturnActuality::SometimesReturns | BlockReturnActuality::NeverReturns,
        ) if return_ty == TastType::unit() => {
            tast_block.push(TypedStmt(TypedStmtKind::ReturnStmt(None).in_span(
                Span::from_positions_and_file(
                    input_block_span.end() - 1,
                    input_block_span.end(),
                    input_block_span.file_name(),
                ),
            )));

            Ok(BlockReturnActuality::AlwaysReturns)
        }

        (
            BlockReturnAbility::MustReturn(_),
            BlockReturnActuality::SometimesReturns | BlockReturnActuality::NeverReturns,
        ) => Err(DiagnosticKind::ExpectedABlockToReturn.error_in(input_block_span)),

        (
            BlockReturnAbility::MustNotReturn,
            BlockReturnActuality::SometimesReturns | BlockReturnActuality::AlwaysReturns,
        ) => {
            panic!(concat!(
                "block must not return, but a sub-block may return",
                " -- this should have been caught when checking that block"
            ));
        }
    }
    .map(|actuality| (tast_block, actuality))
}
