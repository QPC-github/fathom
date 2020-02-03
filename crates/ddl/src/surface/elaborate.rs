//! Elaboration from the surface syntax into the core syntax.
//!
//! Performs the following:
//!
//! - name resolution
//! - desugaring
//! - pattern compilation (TODO)
//! - bidirectional type checking (TODO)
//! - unification (TODO)

use codespan::{FileId, Span};
use codespan_reporting::diagnostic::{Diagnostic, Severity};
use num_bigint::BigInt;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;

use crate::{core, diagnostics, surface};

/// Elaborate a module in the surface syntax into the core syntax.
pub fn elaborate_module(
    globals: &core::Globals,
    surface_module: &surface::Module,
    report: &mut dyn FnMut(Diagnostic),
) -> core::Module {
    let item_context = Context::new(globals, surface_module.file_id);
    core::Module {
        file_id: surface_module.file_id,
        doc: surface_module.doc.clone(),
        items: elaborate_items(item_context, &surface_module.items, report),
    }
}

/// Contextual information to be used during elaboration.
pub struct Context<'me> {
    /// The global environment.
    globals: &'me core::Globals,
    /// The file where these items are defined (for error reporting).
    file_id: FileId,
    /// Labels that have previously been used for items, along with the span
    /// where they were introduced (for error reporting).
    items: HashMap<&'me str, core::Item>,
    /// List of types currently bound in this context. These could either
    /// refer to items or local bindings.
    tys: Vec<(&'me str, Arc<core::Value>)>,
}

impl<'me> Context<'me> {
    /// Create a new context.
    pub fn new(globals: &'me core::Globals, file_id: FileId) -> Context<'me> {
        Context {
            globals,
            file_id,
            items: HashMap::new(),
            tys: Vec::new(),
        }
    }

    /// Lookup the type of a binding corresponding to `name` in the context,
    /// returning `None` if `name` was not yet bound.
    pub fn lookup_ty(&self, name: &str) -> Option<&Arc<core::Value>> {
        Some(&self.tys.iter().rev().find(|(n, _)| *n == name)?.1)
    }
}

/// Elaborate items in the surface syntax into items in the core syntax.
pub fn elaborate_items<'items>(
    mut context: Context<'items>,
    surface_items: &'items [surface::Item],
    report: &mut dyn FnMut(Diagnostic),
) -> Vec<core::Item> {
    let mut core_items = Vec::new();

    for item in surface_items.iter() {
        use std::collections::hash_map::Entry;

        match item {
            surface::Item::Alias(alias) => {
                let (core_term, ty) = match &alias.ty {
                    Some(surface_ty) => {
                        let (core_ty, _) = elaborate_universe(&context, surface_ty, report);
                        let ty = core::semantics::eval(context.globals, &context.items, &core_ty);
                        let core_term = check_term(&context, &alias.term, &ty, report);
                        (core::Term::Ann(Arc::new(core_term), Arc::new(core_ty)), ty)
                    }
                    None => synth_term(&context, &alias.term, report),
                };

                // FIXME: Avoid shadowing builtin definitions
                match context.items.entry(&alias.name.1) {
                    Entry::Vacant(entry) => {
                        let item = core::Alias {
                            span: alias.span,
                            doc: alias.doc.clone(),
                            name: entry.key().to_string(),
                            term: Arc::new(core_term),
                        };

                        let core_item = core::Item::Alias(item);
                        core_items.push(core_item.clone());
                        context.tys.push((*entry.key(), ty));
                        entry.insert(core_item);
                    }
                    Entry::Occupied(entry) => report(diagnostics::item_redefinition(
                        Severity::Error,
                        context.file_id,
                        entry.key(),
                        alias.span,
                        entry.get().span(),
                    )),
                }
            }
            surface::Item::Struct(struct_ty) => {
                let core_fields = elaborate_struct_ty_fields(&context, &struct_ty.fields, report);

                // FIXME: Avoid shadowing builtin definitions
                match context.items.entry(&struct_ty.name.1) {
                    Entry::Vacant(entry) => {
                        let item = core::StructType {
                            span: struct_ty.span,
                            doc: struct_ty.doc.clone(),
                            name: entry.key().to_string(),
                            fields: core_fields,
                        };

                        let core_item = core::Item::Struct(item);
                        core_items.push(core_item.clone());
                        let ty = Arc::new(core::Value::Universe(
                            Span::initial(),
                            core::Universe::Format,
                        ));
                        context.tys.push((*entry.key(), ty));
                        entry.insert(core_item);
                    }
                    Entry::Occupied(entry) => report(diagnostics::item_redefinition(
                        Severity::Error,
                        context.file_id,
                        entry.key(),
                        struct_ty.span,
                        entry.get().span(),
                    )),
                }
            }
        }
    }

    core_items
}

/// Elaborate structure type fields in the surface syntax into structure type
/// fields in the core syntax.
pub fn elaborate_struct_ty_fields(
    context: &Context<'_>,
    surface_fields: &[surface::TypeField],
    report: &mut dyn FnMut(Diagnostic),
) -> Vec<core::TypeField> {
    // Field names that have previously seen, along with the span
    // where they were introduced (for error reporting).
    let mut seen_field_names = HashMap::new();
    // Fields that have been elaborated into the core syntax.
    let mut core_fields = Vec::with_capacity(surface_fields.len());

    for field in surface_fields {
        use std::collections::hash_map::Entry;

        let field_span = Span::merge(field.name.0, field.term.span());
        let format_ty = Arc::new(core::Value::Universe(
            Span::initial(),
            core::Universe::Format,
        ));
        let ty = check_term(&context, &field.term, &format_ty, report);

        match seen_field_names.entry(field.name.1.clone()) {
            Entry::Vacant(entry) => {
                core_fields.push(core::TypeField {
                    doc: field.doc.clone(),
                    start: field_span.start(),
                    name: entry.key().clone(),
                    term: Arc::new(ty),
                });

                entry.insert(field_span);
            }
            Entry::Occupied(entry) => report(diagnostics::field_redeclaration(
                Severity::Error,
                context.file_id,
                entry.key(),
                field_span,
                *entry.get(),
            )),
        }
    }

    core_fields
}

/// Check that a surface term is a type or kind, and elaborate it into the core syntax.
pub fn elaborate_universe(
    context: &Context<'_>,
    surface_term: &surface::Term,
    report: &mut dyn FnMut(Diagnostic),
) -> (core::Term, Option<core::Universe>) {
    use crate::core::Universe::{Format, Host, Kind};

    match surface_term {
        surface::Term::Kind(span) => (core::Term::Universe(*span, Kind), Some(Kind)),
        surface::Term::Host(span) => (core::Term::Universe(*span, Host), Some(Host)),
        surface::Term::Format(span) => (core::Term::Universe(*span, Format), Some(Format)),
        surface_term => {
            let (core_term, ty) = synth_term(context, surface_term, report);
            match ty.as_ref() {
                core::Value::Universe(_, universe) => (core_term, Some(*universe)),
                core::Value::Error(_) => (core_term, None),
                _ => {
                    let span = surface_term.span();
                    report(diagnostics::universe_mismatch(
                        Severity::Error,
                        context.file_id,
                        span,
                        &ty,
                    ));
                    (core::Term::Error(span), None)
                }
            }
        }
    }
}

/// Check a surface term against the given type, and elaborate it into the core syntax.
pub fn check_term(
    context: &Context<'_>,
    surface_term: &surface::Term,
    expected_ty: &Arc<core::Value>,
    report: &mut dyn FnMut(Diagnostic),
) -> core::Term {
    match (surface_term, expected_ty.as_ref()) {
        (surface::Term::Error(span), _) => core::Term::Error(*span),
        (surface_term, core::Value::Error(_)) => core::Term::Error(surface_term.span()),
        (surface::Term::NumberLiteral(span, literal), _) => {
            let error = |report: &mut dyn FnMut(Diagnostic)| {
                report(diagnostics::error::numeric_literal_not_supported(
                    context.file_id,
                    *span,
                    expected_ty,
                ));
                core::Term::Error(surface_term.span())
            };
            match expected_ty.as_ref() {
                // TODO: Lookup globals in environment
                core::Value::Neutral(core::Head::Global(_, name), elims) if elims.is_empty() => {
                    match name.as_str() {
                        "Int" => match literal.parse_big_int(context.file_id, report) {
                            Some(value) => core::Term::Constant(*span, core::Constant::Int(value)),
                            None => core::Term::Error(*span),
                        },
                        "F32" => match literal.parse_float(context.file_id, report) {
                            Some(value) => core::Term::Constant(*span, core::Constant::F32(value)),
                            None => core::Term::Error(*span),
                        },
                        "F64" => match literal.parse_float(context.file_id, report) {
                            Some(value) => core::Term::Constant(*span, core::Constant::F64(value)),
                            None => core::Term::Error(*span),
                        },
                        _ => error(report),
                    }
                }
                _ => error(report),
            }
        }
        (surface::Term::If(span, surface_head, surface_if_true, surface_if_false), _) => {
            // TODO: Lookup globals in environment
            let bool_ty = Arc::new(core::Value::global(Span::initial(), "Bool"));
            let head = check_term(context, surface_head, &bool_ty, report);
            let if_true = check_term(context, surface_if_true, expected_ty, report);
            let if_false = check_term(context, surface_if_false, expected_ty, report);

            core::Term::BoolElim(*span, Arc::new(head), Arc::new(if_true), Arc::new(if_false))
        }
        (surface::Term::Match(span, surface_head, surface_branches), _) => {
            let (head, head_ty) = synth_term(context, surface_head, report);
            let error = |report: &mut dyn FnMut(Diagnostic)| {
                report(diagnostics::error::unsupported_pattern_ty(
                    context.file_id,
                    surface_head.span(),
                    &head_ty,
                ));
                core::Term::Error(*span)
            };

            match head_ty.as_ref() {
                core::Value::Neutral(core::Head::Global(_, name), elims) if elims.is_empty() => {
                    // TODO: Lookup globals in environment
                    match name.as_str() {
                        "Bool" => {
                            let (if_true, if_false) =
                                check_bool_branches(context, surface_branches, expected_ty, report);
                            core::Term::BoolElim(*span, Arc::new(head), if_true, if_false)
                        }
                        "Int" => {
                            let (branches, default) = check_int_branches(
                                context,
                                surface_head.span(),
                                surface_branches,
                                expected_ty,
                                report,
                            );
                            core::Term::IntElim(*span, Arc::new(head), branches, default)
                        }
                        _ => error(report),
                    }
                }
                core::Value::Error(_) => core::Term::Error(*span),
                _ => error(report),
            }
        }
        (surface_term, expected_ty) => {
            let (core_term, synth_ty) = synth_term(context, surface_term, report);

            if core::semantics::equal(&synth_ty, expected_ty) {
                core_term
            } else {
                report(diagnostics::type_mismatch(
                    Severity::Error,
                    context.file_id,
                    surface_term.span(),
                    expected_ty,
                    &synth_ty,
                ));
                core::Term::Error(surface_term.span())
            }
        }
    }
}

/// Synthesize the type of a surface term, and elaborate it into the core syntax.
pub fn synth_term(
    context: &Context<'_>,
    surface_term: &surface::Term,
    report: &mut dyn FnMut(Diagnostic),
) -> (core::Term, Arc<core::Value>) {
    use crate::core::Universe::{Format, Host, Kind};

    match surface_term {
        surface::Term::Ann(surface_term, surface_ty) => {
            let (core_ty, _) = elaborate_universe(context, surface_ty, report);
            let ty = core::semantics::eval(context.globals, &context.items, &core_ty);
            let core_term = check_term(context, surface_term, &ty, report);
            (core::Term::Ann(Arc::new(core_term), Arc::new(core_ty)), ty)
        }
        surface::Term::Name(span, name) => {
            if let Some((ty, _)) = context.globals.get(name) {
                return (
                    core::Term::Global(*span, name.to_owned()),
                    core::semantics::eval(context.globals, &context.items, ty),
                );
            }
            if let Some(ty) = context.lookup_ty(name) {
                return (core::Term::Item(*span, name.to_owned()), ty.clone());
            }

            report(diagnostics::error::var_name_not_found(
                context.file_id,
                name.as_str(),
                *span,
            ));
            (
                core::Term::Error(*span),
                Arc::new(core::Value::Error(Span::initial())),
            )
        }
        surface::Term::Kind(span) => {
            report(diagnostics::kind_has_no_type(
                Severity::Error,
                context.file_id,
                *span,
            ));
            (
                core::Term::Error(*span),
                Arc::new(core::Value::Error(Span::initial())),
            )
        }
        surface::Term::Host(span) => (
            core::Term::Universe(*span, Host),
            Arc::new(core::Value::Universe(Span::initial(), Kind)),
        ),
        surface::Term::Format(span) => (
            core::Term::Universe(*span, Format),
            Arc::new(core::Value::Universe(Span::initial(), Kind)),
        ),
        surface::Term::FunctionType(param_ty, body_ty) => {
            let (core_param_ty, param_universe) = elaborate_universe(context, param_ty, report);
            let (core_body_ty, body_universe) = elaborate_universe(context, body_ty, report);
            let core_fun_ty =
                core::Term::FunctionType(Arc::new(core_param_ty), Arc::new(core_body_ty));

            match (param_universe, body_universe) {
                (Some(Host), Some(Host)) => (
                    core_fun_ty,
                    Arc::new(core::Value::Universe(Span::initial(), Host)),
                ),
                (Some(Host), Some(Kind)) | (Some(Kind), Some(Kind)) => (
                    core_fun_ty,
                    Arc::new(core::Value::Universe(Span::initial(), Kind)),
                ),
                (_, _) => (
                    core::Term::Error(surface_term.span()),
                    Arc::new(core::Value::Error(Span::initial())),
                ),
            }
        }
        surface::Term::FunctionElim(head, arguments) => {
            let span = surface_term.span();
            let (mut core_head, mut head_type) = synth_term(context, head, report);

            for argument in arguments {
                match head_type.as_ref() {
                    core::Value::FunctionType(param_type, body_type) => {
                        core_head = core::Term::FunctionElim(
                            Arc::new(core_head),
                            Arc::new(check_term(context, argument, &param_type, report)),
                        );
                        head_type = body_type.clone();
                    }
                    core::Value::Error(_) => {
                        return (
                            core::Term::Error(span),
                            Arc::new(core::Value::Error(Span::initial())),
                        );
                    }
                    head_ty => {
                        report(diagnostics::not_a_function(
                            Severity::Error,
                            context.file_id,
                            head.span(),
                            head_ty,
                            argument.span(),
                        ));
                        return (
                            core::Term::Error(span),
                            Arc::new(core::Value::Error(Span::initial())),
                        );
                    }
                }
            }

            (core_head, head_type)
        }
        surface::Term::NumberLiteral(span, _) => {
            report(diagnostics::error::ambiguous_numeric_literal(
                context.file_id,
                *span,
            ));

            (
                core::Term::Error(*span),
                Arc::new(core::Value::Error(Span::initial())),
            )
        }
        surface::Term::If(span, surface_head, surface_if_true, surface_if_false) => {
            // TODO: Lookup globals in environment
            let bool_ty = Arc::new(core::Value::global(Span::initial(), "Bool"));
            let head = check_term(context, surface_head, &bool_ty, report);
            let (if_true, if_true_ty) = synth_term(context, surface_if_true, report);
            let (if_false, if_false_ty) = synth_term(context, surface_if_false, report);

            if core::semantics::equal(&if_true_ty, &if_false_ty) {
                (
                    core::Term::BoolElim(
                        *span,
                        Arc::new(head),
                        Arc::new(if_true),
                        Arc::new(if_false),
                    ),
                    if_true_ty,
                )
            } else {
                report(diagnostics::type_mismatch(
                    Severity::Error,
                    context.file_id,
                    surface_if_false.span(),
                    &if_true_ty,
                    &if_false_ty,
                ));
                (
                    core::Term::Error(*span),
                    Arc::new(core::Value::Error(*span)),
                )
            }
        }
        surface::Term::Match(span, _, _) => {
            report(diagnostics::ambiguous_match_expression(
                Severity::Error,
                context.file_id,
                *span,
            ));
            (
                core::Term::Error(*span),
                Arc::new(core::Value::Error(*span)),
            )
        }
        surface::Term::Error(span) => (
            core::Term::Error(*span),
            Arc::new(core::Value::Error(Span::initial())),
        ),
    }
}

#[allow(unused_variables)]
fn check_bool_branches(
    context: &Context<'_>,
    surface_branches: &[(surface::Pattern, surface::Term)],
    expected_ty: &core::Value,
    report: &mut dyn FnMut(Diagnostic),
) -> (Arc<core::Term>, Arc<core::Term>) {
    unimplemented!("boolean eliminators")
}

fn check_int_branches(
    context: &Context<'_>,
    span: Span,
    surface_branches: &[(surface::Pattern, surface::Term)],
    expected_ty: &Arc<core::Value>,
    report: &mut dyn FnMut(Diagnostic),
) -> (BTreeMap<BigInt, Arc<core::Term>>, Arc<core::Term>) {
    use std::collections::btree_map::Entry;

    let mut branches = BTreeMap::new();
    let mut default = None;

    for (pattern, surface_term) in surface_branches {
        match pattern {
            surface::Pattern::NumberLiteral(span, literal) => {
                let core_term = check_term(context, surface_term, expected_ty, report);
                if let Some(value) = literal.parse_big_int(context.file_id, report) {
                    match &default {
                        None => match branches.entry(value) {
                            Entry::Occupied(_) => report(
                                diagnostics::warning::unreachable_pattern(context.file_id, *span),
                            ),
                            Entry::Vacant(entry) => drop(entry.insert(Arc::new(core_term))),
                        },
                        Some(_) => report(diagnostics::warning::unreachable_pattern(
                            context.file_id,
                            *span,
                        )),
                    }
                }
            }
            surface::Pattern::Name(span, _name) => {
                // TODO: check if name is bound
                // - if so compare for equality
                // - otherwise bind local variable
                let core_term = check_term(context, surface_term, expected_ty, report);
                match &default {
                    None => default = Some(Arc::new(core_term)),
                    Some(_) => report(diagnostics::warning::unreachable_pattern(
                        context.file_id,
                        *span,
                    )),
                }
            }
        }
    }

    let default = default.unwrap_or_else(|| {
        report(diagnostics::error::no_default_pattern(
            context.file_id,
            span,
        ));
        Arc::new(core::Term::Error(Span::initial()))
    });

    (branches, default)
}
