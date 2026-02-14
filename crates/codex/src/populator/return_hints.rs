use std::collections::HashSet;

use std::borrow::Cow;

use mago_atom::Atom;
use mago_atom::ascii_lowercase_atom;
use mago_atom::empty_atom;

use crate::metadata::CodebaseMetadata;
use crate::metadata::function_like::ReturnExpressionHint;
use crate::metadata::ttype::TypeMetadata;
use crate::ttype::atomic::TAtomic;
use crate::ttype::atomic::object::TObject;
use crate::ttype::atomic::reference::TReference;
use crate::ttype::union::TUnion;

pub fn resolve_return_expression_hints(codebase: &mut CodebaseMetadata) {
    let keys_with_hints: Vec<(Atom, Atom)> = codebase
        .function_likes
        .iter()
        .filter(|(_, meta)| !meta.return_expression_hints.is_empty() && meta.return_type_metadata.is_none())
        .map(|(key, _)| *key)
        .collect();

    if keys_with_hints.is_empty() {
        return;
    }

    let mut changed = true;
    let max_iterations = 20;
    let mut iteration = 0;

    while changed && iteration < max_iterations {
        changed = false;
        iteration += 1;

        for key in &keys_with_hints {
            if codebase
                .function_likes
                .get(key)
                .map(|m| m.return_type_metadata.is_some())
                .unwrap_or(true)
            {
                continue;
            }

            let hints = match codebase.function_likes.get(key) {
                Some(meta) => meta.return_expression_hints.clone(),
                None => continue,
            };

            let mut resolving: HashSet<(Atom, Atom)> = HashSet::new();
            resolving.insert(*key);

            if let Some(resolved_type) = resolve_hints(&hints, codebase, &mut resolving) {
                if let Some(meta) = codebase.function_likes.get_mut(key) {
                    meta.return_type_metadata = Some(TypeMetadata {
                        span: meta.span,
                        type_union: resolved_type,
                        from_docblock: false,
                        inferred: true,
                    });
                    changed = true;
                }
            }
        }
    }
}

fn resolve_hints(
    hints: &[ReturnExpressionHint],
    codebase: &CodebaseMetadata,
    resolving: &mut HashSet<(Atom, Atom)>,
) -> Option<TUnion> {
    let mut atomics: Vec<TAtomic> = Vec::new();

    for hint in hints {
        match hint {
            ReturnExpressionHint::InstanceMethodCall { class, method }
            | ReturnExpressionHint::StaticMethodCall { class, method } => {
                if let Some(resolved) = resolve_method_return(*class, *method, codebase, resolving) {
                    collect_atomics(&resolved, &mut atomics);
                }
            }
            ReturnExpressionHint::FunctionCall { function } => {
                if let Some(resolved) = resolve_function_return(*function, codebase, resolving) {
                    collect_atomics(&resolved, &mut atomics);
                }
            }
            ReturnExpressionHint::MethodChain {
                receiver_class,
                methods,
            } => {
                if let Some(resolved) = resolve_method_chain(*receiver_class, methods, codebase, resolving) {
                    collect_atomics(&resolved, &mut atomics);
                }
            }
        }
    }

    if atomics.is_empty() {
        None
    } else {
        Some(TUnion::new(Cow::Owned(atomics)))
    }
}

fn resolve_function_return(
    function: Atom,
    codebase: &CodebaseMetadata,
    resolving: &mut HashSet<(Atom, Atom)>,
) -> Option<TUnion> {
    let key = (empty_atom(), function);
    if resolving.contains(&key) {
        return None;
    }

    if let Some(target_meta) = codebase.function_likes.get(&key) {
        if let Some(return_type) = &target_meta.return_type_metadata {
            return Some(return_type.type_union.clone());
        }
        if !target_meta.return_expression_hints.is_empty() {
            resolving.insert(key);
            let result = resolve_hints(&target_meta.return_expression_hints.clone(), codebase, resolving);
            resolving.remove(&key);
            return result;
        }
    }

    let fn_str = function.as_str();
    if let Some(pos) = fn_str.rfind('\\') {
        let short_name = ascii_lowercase_atom(&fn_str[pos + 1..]);
        let short_key = (empty_atom(), short_name);
        if !resolving.contains(&short_key) {
            if let Some(target_meta) = codebase.function_likes.get(&short_key) {
                if let Some(return_type) = &target_meta.return_type_metadata {
                    return Some(return_type.type_union.clone());
                }
                if !target_meta.return_expression_hints.is_empty() {
                    resolving.insert(short_key);
                    let result = resolve_hints(&target_meta.return_expression_hints.clone(), codebase, resolving);
                    resolving.remove(&short_key);
                    return result;
                }
            }
        }
    }

    None
}

fn resolve_method_return(
    class: Atom,
    method: Atom,
    codebase: &CodebaseMetadata,
    resolving: &mut HashSet<(Atom, Atom)>,
) -> Option<TUnion> {
    let key = (class, method);
    if resolving.contains(&key) {
        return None;
    }

    if let Some(target_meta) = codebase.function_likes.get(&key) {
        if let Some(return_type) = &target_meta.return_type_metadata {
            return Some(return_type.type_union.clone());
        }

        if !target_meta.return_expression_hints.is_empty() {
            resolving.insert(key);
            let result = resolve_hints(&target_meta.return_expression_hints.clone(), codebase, resolving);
            resolving.remove(&key);
            return result;
        }
    }

    if let Some(class_meta) = codebase.class_likes.get(&class) {
        for parent in &class_meta.all_parent_classes {
            let parent_key = (*parent, method);
            if resolving.contains(&parent_key) {
                continue;
            }
            if let Some(target_meta) = codebase.function_likes.get(&parent_key) {
                if let Some(return_type) = &target_meta.return_type_metadata {
                    return Some(return_type.type_union.clone());
                }
                if !target_meta.return_expression_hints.is_empty() {
                    resolving.insert(parent_key);
                    let result = resolve_hints(&target_meta.return_expression_hints.clone(), codebase, resolving);
                    resolving.remove(&parent_key);
                    if result.is_some() {
                        return result;
                    }
                }
            }
        }

        for used_trait in &class_meta.used_traits {
            let trait_key = (*used_trait, method);
            if resolving.contains(&trait_key) {
                continue;
            }
            if let Some(target_meta) = codebase.function_likes.get(&trait_key) {
                if let Some(return_type) = &target_meta.return_type_metadata {
                    return Some(return_type.type_union.clone());
                }
                if !target_meta.return_expression_hints.is_empty() {
                    resolving.insert(trait_key);
                    let result = resolve_hints(&target_meta.return_expression_hints.clone(), codebase, resolving);
                    resolving.remove(&trait_key);
                    if result.is_some() {
                        return result;
                    }
                }
            }
        }
    }

    None
}

fn resolve_method_chain(
    receiver_class: Atom,
    methods: &[Atom],
    codebase: &CodebaseMetadata,
    resolving: &mut HashSet<(Atom, Atom)>,
) -> Option<TUnion> {
    if methods.is_empty() {
        return None;
    }

    let mut current_class = receiver_class;

    for (i, method_name) in methods.iter().enumerate() {
        let is_last = i == methods.len() - 1;

        let return_type = resolve_method_return(current_class, *method_name, codebase, resolving);

        match return_type {
            Some(rt) => {
                if is_last {
                    return Some(rt);
                }
                if let Some(next_class) = extract_class_from_type(&rt, current_class) {
                    current_class = next_class;
                } else {
                    return None;
                }
            }
            None => return None,
        }
    }

    None
}

fn collect_atomics(union: &TUnion, atomics: &mut Vec<TAtomic>) {
    for atomic in union.types.iter() {
        if !atomics.contains(atomic) {
            atomics.push(atomic.clone());
        }
    }
}

fn extract_class_from_type(union: &TUnion, current_class: Atom) -> Option<Atom> {
    for atomic in union.types.iter() {
        match atomic {
            TAtomic::Object(TObject::Named(named)) => {
                if named.is_this {
                    return Some(current_class);
                }
                return Some(ascii_lowercase_atom(&named.name));
            }
            TAtomic::Reference(TReference::Symbol {
                name,
                intersection_types: None,
                ..
            }) => {
                return Some(ascii_lowercase_atom(name));
            }
            _ => continue,
        }
    }
    None
}
