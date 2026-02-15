use mago_allocator::Arena;
use mago_names::kind::NameKind;
use mago_names::scope::NamespaceScope;
use mago_phpdoc_syntax::cst::AssertPattern;
use mago_phpdoc_syntax::cst::AssertTagValue;
use mago_phpdoc_syntax::cst::Element;
use mago_phpdoc_syntax::cst::TagValue;
use mago_phpdoc_syntax::cst::TextSegment;
use mago_phpdoc_syntax::cst::r#type::Type;
use mago_reporting::Annotation;
use mago_reporting::Issue;
use mago_span::HasSpan;
use mago_span::Span;
use mago_syntax::cst::ArrowFunction;
use mago_syntax::cst::Block;
use mago_syntax::cst::Closure;
use mago_syntax::cst::ForBody;
use mago_syntax::cst::ForeachBody;
use mago_syntax::cst::Function;
use mago_syntax::cst::IfBody;
use mago_syntax::cst::Method;
use mago_syntax::cst::MethodBody;
use mago_syntax::cst::ModifierSequenceExt;
use mago_syntax::cst::Statement;
use mago_syntax::cst::SwitchBody;
use mago_syntax::cst::SwitchCase;
use mago_syntax::cst::Variable;
use mago_syntax::cst::WhileBody;
use mago_syntax::utils;
use mago_word::Word;
use mago_word::WordMap;
use mago_word::WordSet;
use mago_word::ascii_lowercase_word;
use mago_word::word;

use std::borrow::Cow;

use mago_syntax::ast::Access;
use mago_syntax::ast::Argument;
use mago_syntax::ast::Assignment;
use mago_syntax::ast::AssignmentOperator;
use mago_syntax::ast::Block;
use mago_syntax::ast::Call;
use mago_syntax::ast::ClassLikeMemberSelector;
use mago_syntax::ast::DirectVariable;
use mago_syntax::ast::Expression;
use mago_syntax::ast::ExpressionStatement;
use mago_syntax::ast::Identifier;
use mago_syntax::ast::Literal;
use mago_syntax::ast::PropertyAccess;
use mago_syntax::ast::Statement;
use mago_syntax::ast::Variable;

use crate::metadata::function_like::ReturnExpressionHint;

use crate::metadata::ttype::TypeMetadata;
use crate::ttype::atomic::TAtomic;
use crate::ttype::atomic::object::TObject;
use crate::ttype::atomic::object::named::TNamedObject;
use crate::ttype::union::TUnion;

use crate::assertion::Assertion;
use crate::issue::ScanningIssueKind;
use crate::metadata::class_like::ClassLikeMetadata;
use crate::metadata::constant::ConstantMetadata;
use crate::metadata::flags::MetadataFlags;
use crate::metadata::function_like::FunctionLikeKind;
use crate::metadata::function_like::FunctionLikeMetadata;
use crate::metadata::function_like::MethodMetadata;
use crate::misc::GenericParent;
use crate::scanner::Context;
use crate::scanner::assertion_inference::infer_assertions_from_block_body;
use crate::scanner::assertion_inference::infer_assertions_from_expression_body;
use crate::scanner::attribute::scan_attribute_lists;
use crate::scanner::docblock::apply_common_metadata_flag;
use crate::scanner::docblock::assertion_subject_word;
use crate::scanner::docblock::find_most_trusted_tag;
use crate::scanner::docblock::for_each_tag_by_ascending_trust;
use crate::scanner::docblock::parse_docblock;
use crate::scanner::parameter::scan_function_like_parameter;
use crate::scanner::parameter::scan_function_like_parameter_with_constants;
use crate::scanner::ttype::get_type_metadata_from_hint;
use crate::scanner::ttype::get_type_metadata_from_type;
use crate::scanner::ttype::merge_type_preserving_nullability;
use crate::scanner::typing_error_issue;
use crate::scanner::version_claim::evaluate_version_attributes;
use crate::ttype::builder;
use crate::ttype::get_mixed;
use crate::ttype::resolution::TypeResolutionContext;
use crate::ttype::template::GenericTemplate;
use crate::visibility::Visibility;

#[inline]
pub fn scan_method<'arena, A>(
    functionlike_id: (Word, Word),
    method: &'arena Method<'arena>,
    class_like_metadata: &ClassLikeMetadata,
    context: &mut Context<'_, 'arena, A>,
    scope: &mut NamespaceScope,
    type_resolution_context: Option<TypeResolutionContext>,
) -> Option<FunctionLikeMetadata>
where
    A: Arena,
{
    let span = method.span();

    let mut flags = MetadataFlags::origin_flags(context.file.file_type);

    if method.ampersand.is_some() {
        flags |= MetadataFlags::BY_REFERENCE;
    }

    let verdict = evaluate_version_attributes(&method.attribute_lists, context, context.php_version);

    let lookup_name = ascii_lowercase_word(method.name.value);
    let display_name = word(method.name.value);

    let mut metadata = FunctionLikeMetadata::new(FunctionLikeKind::Method, lookup_name, display_name, span, flags);
    metadata.version_constraint = verdict.constraint;
    metadata.attributes =
        scan_attribute_lists(&method.attribute_lists, context, scope, Some(class_like_metadata.original_name));
    metadata.type_resolution_context = type_resolution_context.filter(|c| !c.is_empty());

    metadata.name_span = Some(method.name.span);
    metadata.parameters = method
        .parameter_list
        .parameters
        .iter()
        .filter_map(|p| scan_function_like_parameter(p, Some(class_like_metadata.original_name), context, scope))
        .collect();

    if let Some(return_hint) = method.return_type_hint.as_ref() {
        metadata.set_return_type_declaration_metadata(Some(get_type_metadata_from_hint(
            &return_hint.hint,
            Some(class_like_metadata.original_name),
            context,
        )));
    }

    let method_name_str = method.name.value;

    let mut method_metadata = MethodMetadata {
        is_final: method.modifiers.contains_final(),
        is_abstract: method.modifiers.contains_abstract(),
        is_static: method.modifiers.contains_static(),
        is_constructor: method_name_str.eq_ignore_ascii_case(b"__construct"),
        visibility: if let Some(v) = method.modifiers.get_first_visibility() {
            Visibility::try_from(v).unwrap_or(Visibility::Public)
        } else {
            Visibility::Public
        },
        where_constraints: WordMap::default(),
    };

    if let MethodBody::Concrete(block) = &method.body {
        if utils::block_has_yield(block) {
            metadata.flags |= MetadataFlags::HAS_YIELD;
        }

        if utils::block_has_throws(block) {
            metadata.flags |= MetadataFlags::HAS_THROW;
        }

        collect_globals_into(block, &mut metadata.globals_accessed);

        if block_has_func_get_args(block) {
            metadata.uses_func_get_args = true;
        }
    } else {
        method_metadata.is_abstract = true;
    }

    metadata.method_metadata = Some(method_metadata);

    scan_function_like_docblock(
        span,
        functionlike_id,
        &mut metadata,
        Some(class_like_metadata.original_name),
        context,
        scope,
    );

    if let MethodBody::Concrete(block) = &method.body {
        infer_assertions_from_block_body(block, &mut metadata, context.resolved_names);
    }

    if metadata.attributes.iter().any(|attr| attr.name.as_bytes().eq_ignore_ascii_case(b"Deprecated")) {
        metadata.flags |= MetadataFlags::DEPRECATED;
    }

    // Automatically mark known fiber-suspending methods.
    if method.name.value.eq_ignore_ascii_case(b"suspend")
        && class_like_metadata.name.as_bytes().eq_ignore_ascii_case(b"revolt\\eventloop\\suspension")
    {
        metadata.flags |= MetadataFlags::SUSPENDS_FIBER;
    }

    // Infer return type from method body when no explicit type is declared.
    if metadata.return_type_metadata.is_none()
        && let MethodBody::Concrete(block) = &method.body
    {
        // First try simple inference (literals, $this, new ClassName)
        if let Some(inferred) = infer_return_type_from_block(block, Some(class_like_metadata.name), context) {
            metadata.set_return_type_metadata(Some(TypeMetadata {
                span,
                type_union: inferred,
                from_docblock: false,
                inferred: true,
            }));
        } else {
            // Store hints for resolution during population phase
            metadata.return_expression_hints = collect_return_expression_hints(
                block,
                Some(class_like_metadata.name),
                context,
            );
        }
    }

    Some(metadata)
}

#[inline]
pub fn scan_function<'arena, A>(
    functionlike_id: (Word, Word),
    function: &'arena Function<'arena>,
    classname: Option<Word>,
    context: &mut Context<'_, 'arena, A>,
    scope: &mut NamespaceScope,
    type_resolution_context: TypeResolutionContext,
    constants: Option<&WordMap<ConstantMetadata>>,
) -> Option<FunctionLikeMetadata>
where
    A: Arena,
{
    let verdict = evaluate_version_attributes(&function.attribute_lists, context, context.php_version);

    let mut flags = MetadataFlags::origin_flags(context.file.file_type);

    if utils::block_has_yield(&function.body) {
        flags |= MetadataFlags::HAS_YIELD;
    }

    if utils::block_has_throws(&function.body) {
        flags |= MetadataFlags::HAS_THROW;
    }

    if function.ampersand.is_some() {
        flags |= MetadataFlags::BY_REFERENCE;
    }

    let name = context.resolved_names.get(&function.name);
    let lookup_name = ascii_lowercase_word(name);
    let display_name = word(name);

    let mut metadata =
        FunctionLikeMetadata::new(FunctionLikeKind::Function, lookup_name, display_name, function.span(), flags);
    metadata.version_constraint = verdict.constraint;
    collect_globals_into(&function.body, &mut metadata.globals_accessed);

    metadata.name_span = Some(function.name.span);
    metadata.parameters = function
        .parameter_list
        .parameters
        .iter()
        .filter_map(|p| scan_function_like_parameter_with_constants(p, classname, context, scope, constants))
        .collect();

    metadata.attributes = scan_attribute_lists(&function.attribute_lists, context, scope, classname);
    metadata.type_resolution_context =
        if type_resolution_context.is_empty() { None } else { Some(type_resolution_context) };

    if block_has_func_get_args(&function.body) {
        metadata.uses_func_get_args = true;
    }

    if let Some(return_hint) = function.return_type_hint.as_ref() {
        metadata.set_return_type_declaration_metadata(Some(get_type_metadata_from_hint(
            &return_hint.hint,
            classname,
            context,
        )));
    }

    scan_function_like_docblock(function.span(), functionlike_id, &mut metadata, classname, context, scope);

    infer_assertions_from_block_body(&function.body, &mut metadata, context.resolved_names);

    if metadata.attributes.iter().any(|attr| attr.name.as_bytes().eq_ignore_ascii_case(b"Deprecated")) {
        metadata.flags |= MetadataFlags::DEPRECATED;
    }

    Some(metadata)
}

#[inline]
pub fn scan_closure<'arena, A>(
    functionlike_id: (Word, Word),
    closure: &'arena Closure<'arena>,
    classname: Option<Word>,
    context: &mut Context<'_, 'arena, A>,
    scope: &mut NamespaceScope,
    type_resolution_context: TypeResolutionContext,
) -> FunctionLikeMetadata
where
    A: Arena,
{
    let span = closure.span();

    let mut flags = MetadataFlags::origin_flags(context.file.file_type);

    if utils::block_has_yield(&closure.body) {
        flags |= MetadataFlags::HAS_YIELD;
    }

    if utils::block_has_throws(&closure.body) {
        flags |= MetadataFlags::HAS_THROW;
    }

    if closure.ampersand.is_some() {
        flags |= MetadataFlags::BY_REFERENCE;
    }

    let synthetic_name = functionlike_id.1;

    let mut metadata =
        FunctionLikeMetadata::new(FunctionLikeKind::Closure, synthetic_name, synthetic_name, span, flags)
            .with_parameters(
                closure
                    .parameter_list
                    .parameters
                    .iter()
                    .filter_map(|p| scan_function_like_parameter(p, classname, context, scope)),
            );
    collect_globals_into(&closure.body, &mut metadata.globals_accessed);

    metadata.attributes = scan_attribute_lists(&closure.attribute_lists, context, scope, classname);
    metadata.type_resolution_context =
        if type_resolution_context.is_empty() { None } else { Some(type_resolution_context) };

    if let Some(return_hint) = closure.return_type_hint.as_ref() {
        metadata.set_return_type_declaration_metadata(Some(get_type_metadata_from_hint(
            &return_hint.hint,
            classname,
            context,
        )));
    }

    scan_function_like_docblock(span, functionlike_id, &mut metadata, classname, context, scope);

    infer_assertions_from_block_body(&closure.body, &mut metadata, context.resolved_names);

    metadata
}

#[inline]
pub fn scan_arrow_function<'arena, A>(
    functionlike_id: (Word, Word),
    arrow_function: &'arena ArrowFunction<'arena>,
    classname: Option<Word>,
    context: &mut Context<'_, 'arena, A>,
    scope: &mut NamespaceScope,
    type_resolution_context: TypeResolutionContext,
) -> FunctionLikeMetadata
where
    A: Arena,
{
    let span = arrow_function.span();

    let mut flags = MetadataFlags::origin_flags(context.file.file_type);

    if utils::expression_has_yield(arrow_function.expression) {
        flags |= MetadataFlags::HAS_YIELD;
    }

    if utils::expression_has_throws(arrow_function.expression) {
        flags |= MetadataFlags::HAS_THROW;
    }

    if arrow_function.ampersand.is_some() {
        flags |= MetadataFlags::BY_REFERENCE;
    }

    let synthetic_name = functionlike_id.1;

    let mut metadata =
        FunctionLikeMetadata::new(FunctionLikeKind::ArrowFunction, synthetic_name, synthetic_name, span, flags)
            .with_parameters(
                arrow_function
                    .parameter_list
                    .parameters
                    .iter()
                    .filter_map(|p| scan_function_like_parameter(p, classname, context, scope)),
            );

    metadata.attributes = scan_attribute_lists(&arrow_function.attribute_lists, context, scope, classname);
    metadata.type_resolution_context =
        if type_resolution_context.is_empty() { None } else { Some(type_resolution_context) };

    if let Some(return_hint) = arrow_function.return_type_hint.as_ref() {
        metadata.set_return_type_declaration_metadata(Some(get_type_metadata_from_hint(
            &return_hint.hint,
            classname,
            context,
        )));
    }

    scan_function_like_docblock(span, functionlike_id, &mut metadata, classname, context, scope);

    infer_assertions_from_expression_body(arrow_function.expression, &mut metadata, context.resolved_names);

    metadata
}

fn scan_function_like_docblock<A>(
    span: Span,
    functionlike_id: (Word, Word),
    metadata: &mut FunctionLikeMetadata,
    classname: Option<Word>,
    context: &Context<'_, '_, A>,
    scope: &mut NamespaceScope,
) where
    A: Arena,
{
    let Some(document) = parse_docblock(context, span) else {
        metadata.has_docblock = false;

        return;
    };

    metadata.has_docblock = true;

    for parse_error in document.errors {
        metadata.issues.push(
            Issue::error("Failed to parse function-like docblock comment.")
                .with_code(ScanningIssueKind::MalformedDocblockComment)
                .with_annotation(Annotation::primary(parse_error.span()).with_message(parse_error.to_string()))
                .with_note(parse_error.note())
                .with_help(parse_error.help()),
        );
    }

    let mut ignore_nullable_return = false;
    let mut ignore_falsable_return = false;

    for element in document.elements.iter() {
        let tag = match element {
            Element::Tag(tag) => tag,
            Element::Text(text) => {
                let has_inline_inherit_doc = text.segments.iter().any(|segment| {
                    matches!(
                        segment,
                        TextSegment::InlineTag(inline_tag)
                            if matches!(inline_tag.tag.value, TagValue::InheritDoc(_))
                    )
                });

                if has_inline_inherit_doc {
                    metadata.flags |= MetadataFlags::INHERITS_DOCS;
                }

                continue;
            }
            Element::Code(_) => continue,
        };

        if apply_common_metadata_flag(&mut metadata.flags, &tag.value) {
            continue;
        }

        match &tag.value {
            TagValue::MustUse(_) => {
                metadata.flags |= MetadataFlags::MUST_USE;
            }
            TagValue::Pure(_) => {
                metadata.flags |= MetadataFlags::PURE;
            }
            TagValue::Impure(_) => {
                metadata.flags.set(MetadataFlags::PURE, false);
            }
            TagValue::MutationFree(_) => {
                metadata.flags |= MetadataFlags::MUTATION_FREE;
                metadata.flags |= MetadataFlags::EXTERNAL_MUTATION_FREE;
            }
            TagValue::ExternalMutationFree(_) => {
                metadata.flags |= MetadataFlags::EXTERNAL_MUTATION_FREE;
            }
            TagValue::SuspendsFiber(_) => {
                metadata.flags |= MetadataFlags::SUSPENDS_FIBER;
            }
            TagValue::IgnoreNullableReturn(_) => {
                metadata.flags |= MetadataFlags::IGNORE_NULLABLE_RETURN;
                ignore_nullable_return = true;
            }
            TagValue::IgnoreFalsableReturn(_) => {
                metadata.flags |= MetadataFlags::IGNORE_FALSABLE_RETURN;
                ignore_falsable_return = true;
            }
            TagValue::InheritDoc(_) => {
                metadata.flags |= MetadataFlags::INHERITS_DOCS;
            }
            TagValue::NoNamedArguments(_) => {
                metadata.flags |= MetadataFlags::NO_NAMED_ARGUMENTS;
            }
            _ => {}
        }
    }

    for tag in document.tags() {
        if let TagValue::Template(template) = &tag.value {
            scope.add(NameKind::Default, template.name.value, &(None as Option<&str>));
        }
    }

    let mut type_context = metadata.type_resolution_context.clone().unwrap_or_default();
    for tag in document.tags() {
        let TagValue::Template(template) = &tag.value else {
            continue;
        };

        let template_name = word(template.name.value);
        let template_as_type = if let Some(bound) = &template.bound {
            match builder::get_union_from_type(bound.r#type, scope, &type_context, classname) {
                Ok(tunion) => tunion,
                Err(typing_error) => {
                    metadata.issues.push(typing_error_issue(
                        "Invalid `@template` type.",
                        ScanningIssueKind::InvalidTemplateTag,
                        &typing_error,
                    ));

                    continue;
                }
            }
        } else {
            get_mixed()
        };

        let template_default = if let Some(default) = &template.default {
            match builder::get_union_from_type(default.r#type, scope, &type_context, classname) {
                Ok(tunion) => Some(tunion),
                Err(typing_error) => {
                    metadata.issues.push(typing_error_issue(
                        "Invalid `@template` default type.",
                        ScanningIssueKind::InvalidTemplateTag,
                        &typing_error,
                    ));

                    None
                }
            }
        } else {
            None
        };

        let definition = GenericTemplate::new(GenericParent::FunctionLike(functionlike_id), template_as_type)
            .with_default(template_default);

        metadata.add_template_type(template_name, definition.clone());
        type_context = type_context.with_template_definition(template_name, vec![definition]);
    }

    for_each_tag_by_ascending_trust(&document, |tag| {
        let TagValue::Param(parameter_tag) = &tag.value else {
            return;
        };

        let Some(parameter_variable) = parameter_tag.parameter else {
            return;
        };

        let parameter_name = word(parameter_variable.value);
        let is_variadic = parameter_tag.is_variadic();

        let Some(parameter_metadata) = metadata.get_parameter_mut(parameter_name) else {
            metadata.issues.push(
                Issue::error("The @param tag references an unknown parameter.")
                    .with_code(ScanningIssueKind::InvalidParamTag)
                    .with_annotation(
                        Annotation::primary(parameter_tag.span())
                            .with_message(format!("Parameter `{parameter_variable}` is not defined in this function")),
                    )
                    .with_note(
                        "Each `@param` tag in a docblock must correspond to a parameter in the function's signature.",
                    )
                    .with_help("Please check for typos or add the parameter to the function signature."),
            );

            return;
        };

        let mut variadic_mismatch_issue = None;
        if is_variadic && !parameter_metadata.flags.is_variadic() {
            let parameter_span = parameter_metadata.get_span();
            parameter_metadata.flags |= MetadataFlags::VARIADIC;

            variadic_mismatch_issue = Some(
                Issue::error("@param tag has a variadic mismatch.")
                    .with_code(ScanningIssueKind::InvalidParamTag)
                    .with_annotation(Annotation::primary(parameter_tag.span()).with_message(
                        "This docblock declares the parameter as variadic, but the function signature does not",
                    ))
                    .with_annotation(
                        Annotation::secondary(parameter_span)
                            .with_message("The parameter is declared here without being variadic"),
                    )
                    .with_note("The use of `...` in the `@param` tag must match the function's parameter declaration.")
                    .with_help("Either add `...` to the parameter in the function signature or remove it from the `@param` tag."),
            );
        }

        match get_type_metadata_from_type(parameter_tag.r#type, classname, &type_context, scope) {
            Ok(mut provided_type) => {
                let resulting_type = if !is_variadic
                    && parameter_metadata.flags.is_variadic()
                    && let Some(array_value) = provided_type.type_union.get_single_value_of_array_like()
                {
                    provided_type.type_union = array_value.into_owned();
                    provided_type
                } else {
                    provided_type
                };

                let real_type = parameter_metadata.type_metadata.as_ref();
                let resulting_type = merge_type_preserving_nullability(resulting_type, real_type);

                parameter_metadata.set_type_metadata(Some(resulting_type));
            }
            Err(typing_error) => {
                metadata.issues.push(typing_error_issue(
                    "Could not resolve the type for the @param tag.",
                    ScanningIssueKind::InvalidParamTag,
                    &typing_error,
                ));
            }
        }

        if let Some(variadic_mismatch_issue) = variadic_mismatch_issue {
            metadata.issues.push(variadic_mismatch_issue);
        }
    });

    for tag in document.tags() {
        let TagValue::ParamOut(param_out) = &tag.value else {
            continue;
        };

        let param_name = word(param_out.parameter.value);

        let Some(parameter_metadata) = metadata.get_parameter_mut(param_name) else {
            metadata.issues.push(
                Issue::error("@param-out tag references an unknown parameter.")
                    .with_code(ScanningIssueKind::InvalidParamOutTag)
                    .with_annotation(
                        Annotation::primary(param_out.span())
                            .with_message(format!("Parameter `{}` does not exist", param_out.parameter)),
                    )
                    .with_note("The `@param-out` tag specifies the type of a by-reference parameter after the function has executed.")
                    .with_help("Check for typos or ensure this parameter exists in the function signature."),
            );

            continue;
        };

        if !parameter_metadata.flags.is_by_reference() {
            metadata.issues.push(
                Issue::error("@param-out tag used on a non-by-reference parameter")
                    .with_code(ScanningIssueKind::InvalidParamOutTag)
                    .with_annotation(
                        Annotation::primary(param_out.span())
                            .with_message("This parameter is not declared as by-reference"),
                    )
                    .with_note("The `@param-out` tag can only be used with parameters that are passed by reference.")
                    .with_help("Ensure the parameter is declared with `&` in the function signature."),
            );

            continue;
        }

        match get_type_metadata_from_type(param_out.r#type, classname, &type_context, scope) {
            Ok(parameter_out_type) => {
                parameter_metadata.out_type = Some(parameter_out_type);
            }
            Err(typing_error) => {
                metadata.issues.push(typing_error_issue(
                    "Invalid `@param-out` type string.",
                    ScanningIssueKind::InvalidParamOutTag,
                    &typing_error,
                ));
            }
        }
    }

    for tag in document.tags() {
        let TagValue::ParamClosureThis(param_closure_this) = &tag.value else {
            continue;
        };

        let param_name = word(param_closure_this.parameter.value);

        let Some(parameter_metadata) = metadata.get_parameter_mut(param_name) else {
            metadata.issues.push(
                Issue::error("@param-closure-this tag references an unknown parameter.")
                    .with_code(ScanningIssueKind::InvalidParamClosureThisTag)
                    .with_annotation(
                        Annotation::primary(param_closure_this.span())
                            .with_message(format!("Parameter `{}` does not exist", param_closure_this.parameter)),
                    )
                    .with_note("The `@param-closure-this` tag specifies the type of `$this` bound inside a closure passed as this parameter.")
                    .with_help("Check for typos or ensure this parameter exists in the function signature."),
            );

            continue;
        };

        match get_type_metadata_from_type(param_closure_this.r#type, classname, &type_context, scope) {
            Ok(closure_this_type) => {
                parameter_metadata.closure_this_type = Some(closure_this_type);
            }
            Err(typing_error) => {
                metadata.issues.push(typing_error_issue(
                    "Invalid `@param-closure-this` type string.",
                    ScanningIssueKind::InvalidParamClosureThisTag,
                    &typing_error,
                ));
            }
        }
    }

    let return_tag = find_most_trusted_tag(&document, |tag| match &tag.value {
        TagValue::Return(return_tag) => Some(*return_tag),
        _ => None,
    });

    if let Some(return_type) = return_tag.as_ref() {
        match get_type_metadata_from_type(return_type.r#type, classname, &type_context, scope) {
            Ok(return_type_signature) => {
                metadata.set_return_type_metadata(Some(return_type_signature));
            }
            Err(typing_error) => {
                metadata.issues.push(typing_error_issue(
                    "Failed to resolve `@return` type string.",
                    ScanningIssueKind::InvalidReturnTag,
                    &typing_error,
                ));
            }
        }
    }

    for tag in document.tags() {
        let TagValue::Where(where_tag) = &tag.value else {
            continue;
        };

        let Some(method_metadata) = metadata.get_method_metadata_mut() else {
            metadata.issues.push(
                Issue::error("`@where` tag cannot be used on functions or closures.")
                    .with_code(ScanningIssueKind::InvalidWhereTag)
                    .with_annotation(
                        Annotation::primary(where_tag.span())
                            .with_message("`@where` is only valid on instance methods"),
                    )
                    .with_note("The `@where` tag constrains template types based on the instance type of `$this`. Functions and closures do not have a `$this` context.")
                    .with_help("Remove the `@where` tag. If you need this logic, consider refactoring it into an instance method on a class."),
            );

            continue;
        };

        if method_metadata.is_static {
            metadata.issues.push(
                Issue::error("`@where` tag cannot be used on static methods.")
                    .with_code(ScanningIssueKind::InvalidWhereTag)
                    .with_annotation(
                        Annotation::primary(where_tag.span())
                            .with_message("This constraint is not allowed on a static method"),
                    )
                    .with_note("The `@where` tag constrains template types based on the instance type of `$this`. Static methods are not tied to an instance and have no `$this` context.")
                    .with_help("Remove the `@where` tag. To constrain a template type on a static method, use a type bound like `@template T of SomeInterface` instead."),
            );

            continue;
        }

        match get_type_metadata_from_type(where_tag.r#type, classname, &type_context, scope) {
            Ok(constraint_type) => {
                let template_name = word(where_tag.name.value);

                method_metadata.where_constraints.insert(template_name, constraint_type);
            }
            Err(typing_error) => metadata.issues.push(
                Issue::error(format!("Invalid constraint type `{}` in `@where` tag.", where_tag.r#type))
                    .with_code(ScanningIssueKind::InvalidWhereTag)
                    .with_annotation(Annotation::primary(typing_error.span()).with_message(typing_error.to_string()))
                    .with_note(typing_error.note())
                    .with_help(typing_error.help()),
            ),
        }
    }

    for tag in document.tags() {
        let TagValue::Throws(thrown) = &tag.value else {
            continue;
        };

        match get_type_metadata_from_type(thrown.r#type, classname, &type_context, scope) {
            Ok(thrown_type) => {
                metadata.thrown_types.push(thrown_type);
            }
            Err(typing_error) => {
                metadata.issues.push(typing_error_issue(
                    "Invalid `@throws` type string.",
                    ScanningIssueKind::InvalidThrowsTag,
                    &typing_error,
                ));
            }
        }
    }

    for tag in document.tags() {
        let (TagValue::Assert(assertion_tag)
        | TagValue::AssertIfTrue(assertion_tag)
        | TagValue::AssertIfFalse(assertion_tag)) = &tag.value
        else {
            continue;
        };

        let assertion_subject = assertion_subject_word(&assertion_tag.subject);
        let assertions = parse_assertions_from_tag(assertion_tag, classname, &type_context, scope, metadata);

        let bucket = match &tag.value {
            TagValue::AssertIfTrue(_) => &mut metadata.if_true_assertions,
            TagValue::AssertIfFalse(_) => &mut metadata.if_false_assertions,
            _ => &mut metadata.assertions,
        };

        for assertion in assertions {
            bucket.entry(assertion_subject).or_default().push(assertion);
        }
    }

    metadata.type_resolution_context = Some(type_context);

    if ignore_nullable_return || ignore_falsable_return {
        if let Some(return_type) = &mut metadata.return_type_metadata {
            return_type.type_union.set_ignore_nullable_issues(ignore_nullable_return);
            return_type.type_union.set_ignore_falsable_issues(ignore_falsable_return);
        }

        if let Some(return_type) = &mut metadata.return_type_declaration_metadata {
            return_type.type_union.set_ignore_nullable_issues(ignore_nullable_return);
            return_type.type_union.set_ignore_falsable_issues(ignore_falsable_return);
        }
    }
}

fn parse_assertions_from_tag(
    assertion_tag: &AssertTagValue<'_>,
    classname: Option<Word>,
    type_context: &TypeResolutionContext,
    scope: &NamespaceScope,
    function_like_metadata: &mut FunctionLikeMetadata,
) -> Vec<Assertion> {
    let mut assertions = Vec::new();

    let is_negation = assertion_tag.is_negated();
    let is_equal = assertion_tag.is_equality();

    let asserted_type = match &assertion_tag.pattern {
        AssertPattern::Truthy(_) => {
            assertions.push(if is_negation { Assertion::Falsy } else { Assertion::Truthy });

            return assertions;
        }
        AssertPattern::Falsy(_) => {
            assertions.push(if is_negation { Assertion::Truthy } else { Assertion::Falsy });

            return assertions;
        }
        AssertPattern::NonEmpty(_) => {
            assertions.push(if is_negation { Assertion::Empty } else { Assertion::NonEmpty });

            return assertions;
        }
        AssertPattern::Type(asserted_type) => asserted_type,
    };

    if !is_equal && matches!(asserted_type, Type::Empty(_)) {
        assertions.push(if is_negation { Assertion::NonEmpty } else { Assertion::Empty });

        return assertions;
    }

    match get_type_metadata_from_type(asserted_type, classname, type_context, scope) {
        Ok(type_metadata) => match (is_equal, is_negation) {
            (true, true) => {
                for atomic in type_metadata.type_union.types.into_owned() {
                    assertions.push(Assertion::IsNotIdentical(atomic));
                }
            }
            (true, false) => {
                for atomic in type_metadata.type_union.types.into_owned() {
                    assertions.push(Assertion::IsIdentical(atomic));
                }
            }
            (false, true) => {
                for atomic in type_metadata.type_union.types.into_owned() {
                    assertions.push(Assertion::IsNotType(atomic));
                }
            }
            (false, false) => {
                for atomic in type_metadata.type_union.types.into_owned() {
                    assertions.push(Assertion::IsType(atomic));
                }
            }
        },
        Err(typing_error) => {
            function_like_metadata.issues.push(typing_error_issue(
                "Failed to resolve assertion type string.",
                ScanningIssueKind::InvalidAssertionTag,
                &typing_error,
            ));
        }
    }

    assertions
}

/// Collects every variable imported via `global $x;` anywhere in `block`, without
/// descending into nested function/closure/arrow-function definitions (those are
/// separate scopes).
pub fn collect_globals_into(block: &Block, globals: &mut WordSet) {
    for statement in &block.statements {
        collect_globals_from_statement(statement, globals);
    }
}

fn collect_globals_from_statement(statement: &Statement, globals: &mut WordSet) {
    match statement {
        Statement::Global(global) => {
            for variable in &global.variables {
                if let Variable::Direct(direct) = variable {
                    globals.insert(word(direct.name));
                }
            }
        }
        Statement::Block(block) => collect_globals_into(block, globals),
        Statement::Namespace(namespace) => {
            for statement in namespace.statements() {
                collect_globals_from_statement(statement, globals);
            }
        }
        Statement::If(r#if) => match &r#if.body {
            IfBody::Statement(body) => {
                collect_globals_from_statement(body.statement, globals);
                for else_if in &body.else_if_clauses {
                    collect_globals_from_statement(else_if.statement, globals);
                }
                if let Some(else_clause) = &body.else_clause {
                    collect_globals_from_statement(else_clause.statement, globals);
                }
            }
            IfBody::ColonDelimited(body) => {
                for statement in &body.statements {
                    collect_globals_from_statement(statement, globals);
                }
                for else_if in &body.else_if_clauses {
                    for statement in &else_if.statements {
                        collect_globals_from_statement(statement, globals);
                    }
                }
                if let Some(else_clause) = &body.else_clause {
                    for statement in &else_clause.statements {
                        collect_globals_from_statement(statement, globals);
                    }
                }
            }
        },
        Statement::For(r#for) => match &r#for.body {
            ForBody::Statement(statement) => collect_globals_from_statement(statement, globals),
            ForBody::ColonDelimited(body) => {
                for statement in &body.statements {
                    collect_globals_from_statement(statement, globals);
                }
            }
        },
        Statement::Foreach(foreach) => match &foreach.body {
            ForeachBody::Statement(statement) => collect_globals_from_statement(statement, globals),
            ForeachBody::ColonDelimited(body) => {
                for statement in &body.statements {
                    collect_globals_from_statement(statement, globals);
                }
            }
        },
        Statement::While(r#while) => match &r#while.body {
            WhileBody::Statement(statement) => collect_globals_from_statement(statement, globals),
            WhileBody::ColonDelimited(body) => {
                for statement in &body.statements {
                    collect_globals_from_statement(statement, globals);
                }
            }
        },
        Statement::DoWhile(do_while) => collect_globals_from_statement(do_while.statement, globals),
        Statement::Switch(switch) => {
            let cases = match &switch.body {
                SwitchBody::BraceDelimited(body) => &body.cases,
                SwitchBody::ColonDelimited(body) => &body.cases,
            };
            for case in cases {
                match case {
                    SwitchCase::Expression(case) => {
                        for statement in &case.statements {
                            collect_globals_from_statement(statement, globals);
                        }
                    }
                    SwitchCase::Default(case) => {
                        for statement in &case.statements {
                            collect_globals_from_statement(statement, globals);
                        }
                    }
                }
            }
        }
        Statement::Try(r#try) => {
            for statement in &r#try.block.statements {
                collect_globals_from_statement(statement, globals);
            }
            for catch in &r#try.catch_clauses {
                for statement in &catch.block.statements {
                    collect_globals_from_statement(statement, globals);
                }
            }
            if let Some(finally) = &r#try.finally_clause {
                for statement in &finally.block.statements {
                    collect_globals_from_statement(statement, globals);
                }
            }
        }
        _ => {}
    }
}

/// Attempts to infer a return type from a block by examining all return statements.
///
/// Returns `Some(TUnion)` when every return expression maps to a recognizable type:
/// - `return $this;`         → `static` (TNamedObject with is_this=true)
/// - `return true;`/`false;` → `bool`
/// - `return 123;`           → `int`
/// - `return 1.5;`           → `float`
/// - `return 'str';`         → `string`
/// - `return null;`          → `null`
///
/// Returns `None` if the block has no returns or any return expression is too complex.
fn infer_return_type_from_block<'arena>(
    block: &Block<'_>,
    classname: Option<Atom>,
    context: &Context<'_, 'arena>,
) -> Option<TUnion> {
    let returns = utils::find_returns_in_block(block);
    if returns.is_empty() {
        return None;
    }

    let mut atomics: Vec<TAtomic> = Vec::new();

    for ret in &returns {
        let Some(expr) = ret.value else {
            continue;
        };

        let Some(atomic) = infer_atomic_from_expression(expr, classname, context, Some(block)) else {
            continue;
        };

        if !atomics.iter().any(|existing| *existing == atomic) {
            atomics.push(atomic);
        }
    }

    if atomics.is_empty() {
        return None;
    }

    Some(TUnion::new(Cow::Owned(atomics)))
}

/// Maps a simple expression to a TAtomic type, or None if too complex.
fn infer_atomic_from_expression<'arena>(
    expr: &Expression<'_>,
    classname: Option<Atom>,
    context: &Context<'_, 'arena>,
    block: Option<&Block<'_>>,
) -> Option<TAtomic> {
    use crate::ttype::shared;

    match expr {
        Expression::Variable(Variable::Direct(direct)) if direct.name.eq_ignore_ascii_case("$this") => {
            let name = classname?;
            let mut named = TNamedObject::new(name);
            named.is_this = true;
            Some(TAtomic::Object(TObject::Named(named)))
        }
        Expression::Variable(Variable::Direct(direct)) => {
            let block = block?;
            let assigned_expr = find_last_assignment_in_block(block, direct.name)?;
            infer_atomic_from_expression(assigned_expr, classname, context, None)
        }
        Expression::Literal(Literal::True(_) | Literal::False(_)) => Some(shared::BOOL_ATOMIC.clone()),
        Expression::Literal(Literal::Integer(_)) => Some(shared::INT_ATOMIC.clone()),
        Expression::Literal(Literal::Float(_)) => Some(shared::FLOAT_ATOMIC.clone()),
        Expression::Literal(Literal::String(_)) => Some(shared::STRING_ATOMIC.clone()),
        Expression::Literal(Literal::Null(_)) => Some(shared::NULL_ATOMIC.clone()),
        Expression::Instantiation(instantiation) => {
            match &instantiation.class {
                Expression::Identifier(Identifier::Local(local)) => {
                    let resolved_name = context.resolved_names.get(&local.span);
                    let class_atom = atom(resolved_name);
                    Some(TAtomic::Object(TObject::Named(TNamedObject::new(class_atom))))
                }
                Expression::Identifier(Identifier::Qualified(qualified)) => {
                    let class_atom = atom(qualified.value);
                    Some(TAtomic::Object(TObject::Named(TNamedObject::new(class_atom))))
                }
                Expression::Identifier(Identifier::FullyQualified(fully_qualified)) => {
                    let class_atom = atom(fully_qualified.value);
                    Some(TAtomic::Object(TObject::Named(TNamedObject::new(class_atom))))
                }
                _ => None,
            }
        }
        Expression::Call(_) => {
            None
        }
        _ => None,
    }
}

fn find_last_assignment_in_block<'a, 'arena>(
    block: &'a Block<'arena>,
    var_name: &str,
) -> Option<&'a Expression<'arena>> {
    let mut last_rhs: Option<&'a Expression<'arena>> = None;
    for stmt in block.statements.iter() {
        if let Statement::Expression(ExpressionStatement {
            expression: Expression::Assignment(Assignment {
                lhs: Expression::Variable(Variable::Direct(direct)),
                operator: AssignmentOperator::Assign(_),
                rhs,
                ..
            }),
            ..
        }) = stmt {
            if direct.name.eq_ignore_ascii_case(var_name) {
                last_rhs = Some(rhs);
            }
        }
    }
    last_rhs
}

/// Collects return expression hints from a method body for later resolution.
/// These hints are used by the populator phase to infer return types from method calls.
pub fn collect_return_expression_hints(
    block: &mago_syntax::ast::Block<'_>,
    current_classname: Option<Atom>,
    context: &Context<'_, '_>,
) -> Vec<ReturnExpressionHint> {
    let returns = utils::find_returns_in_block(block);
    if returns.is_empty() {
        return vec![];
    }

    let mut hints: Vec<ReturnExpressionHint> = Vec::new();

    for ret in &returns {
        let Some(expr) = ret.value else {
            continue;
        };

        if let Some(hint) = extract_return_hint(expr, current_classname, context, Some(block)) {
            if !hints.contains(&hint) {
                hints.push(hint);
            }
        }
    }

    hints
}

/// Extracts a return expression hint from a single return expression.
fn extract_return_hint(
    expr: &Expression<'_>,
    current_classname: Option<Atom>,
    context: &Context<'_, '_>,
    block: Option<&Block<'_>>,
) -> Option<ReturnExpressionHint> {
    match expr {
        Expression::Variable(Variable::Direct(direct)) if !direct.name.eq_ignore_ascii_case("$this") => {
            let block = block?;
            let assigned_expr = find_last_assignment_in_block(block, direct.name)?;
            extract_return_hint(assigned_expr, current_classname, context, None)
        }
        Expression::Access(Access::Property(PropertyAccess { object, property, .. })) => {
            if let Expression::Variable(Variable::Direct(DirectVariable { name, .. })) = object {
                if name.eq_ignore_ascii_case("$this") {
                    let class_name = current_classname?;
                    let property_name = match property {
                        ClassLikeMemberSelector::Identifier(ident) => {
                            // Property names in metadata include the '$' prefix.
                            // Use atom() not ascii_lowercase_atom() because PHP property names are case-sensitive.
                            let name_with_dollar = format!("${}", ident.value);
                            atom(&name_with_dollar)
                        },
                        _ => return None,
                    };
                    return Some(ReturnExpressionHint::PropertyAccess {
                        class: class_name,
                        property: property_name,
                    });
                }
            }
            None
        }
        Expression::Call(Call::Method(method_call)) => {
            // Handle: return $this->method(...);
            if let Expression::Variable(Variable::Direct(direct)) = &method_call.object {
                if direct.name.eq_ignore_ascii_case("$this") {
                    let class_name = current_classname?;
                    let method_name = match &method_call.method {
                        ClassLikeMemberSelector::Identifier(ident) => ascii_lowercase_atom(ident.value),
                        _ => return None,
                    };
                    return Some(ReturnExpressionHint::InstanceMethodCall {
                        class: class_name,
                        method: method_name,
                    });
                }
            }

            // Handle: return $this->a()->b()->c() or $obj->a()->b()
            // Try to extract a method chain starting from the receiver
            if let Some((receiver_class, methods)) = extract_method_chain_from_expression(&method_call.object, current_classname, context) {
                let method_name = match &method_call.method {
                    ClassLikeMemberSelector::Identifier(ident) => ascii_lowercase_atom(ident.value),
                    _ => return None,
                };
                let mut all_methods = methods;
                all_methods.push(method_name);
                return Some(ReturnExpressionHint::MethodChain {
                    receiver_class,
                    methods: all_methods.into_boxed_slice(),
                });
            }

            None
        }
        Expression::Call(Call::StaticMethod(static_call)) => {
            // Check if this is the start of a chain: \XF::app()->language()
            // First, try to extract a simple static method call
            let (class_name, method_name) = match &static_call.class {
                // static::method() - parsed as Expression::Static, not Identifier
                Expression::Static(_) => {
                    let class_name = current_classname?;
                    let method_name = match &static_call.method {
                        ClassLikeMemberSelector::Identifier(ident) => ascii_lowercase_atom(ident.value),
                        _ => return None,
                    };
                    (class_name, method_name)
                }
                // self::method() - parsed as Expression::Self_, not Identifier
                Expression::Self_(_) => {
                    let class_name = current_classname?;
                    let method_name = match &static_call.method {
                        ClassLikeMemberSelector::Identifier(ident) => ascii_lowercase_atom(ident.value),
                        _ => return None,
                    };
                    (class_name, method_name)
                }
                // parent::method()
                Expression::Parent(_) => {
                    let class_name = current_classname?;
                    let method_name = match &static_call.method {
                        ClassLikeMemberSelector::Identifier(ident) => ascii_lowercase_atom(ident.value),
                        _ => return None,
                    };
                    (class_name, method_name)
                }
                // ClassName::method()
                Expression::Identifier(Identifier::Local(local)) => {
                    let resolved_name = context.resolved_names.get(&local.span);
                    let class_atom = ascii_lowercase_atom(resolved_name);
                    let method_name = match &static_call.method {
                        ClassLikeMemberSelector::Identifier(ident) => ascii_lowercase_atom(ident.value),
                        _ => return None,
                    };
                    (class_atom, method_name)
                }
                Expression::Identifier(Identifier::Qualified(qualified)) => {
                    let class_atom = ascii_lowercase_atom(qualified.value);
                    let method_name = match &static_call.method {
                        ClassLikeMemberSelector::Identifier(ident) => ascii_lowercase_atom(ident.value),
                        _ => return None,
                    };
                    (class_atom, method_name)
                }
                Expression::Identifier(Identifier::FullyQualified(fully_qualified)) => {
                    let value = fully_qualified.value;
                    let stripped = if value.starts_with('\\') { &value[1..] } else { value };
                    let class_atom = ascii_lowercase_atom(stripped);
                    let method_name = match &static_call.method {
                        ClassLikeMemberSelector::Identifier(ident) => ascii_lowercase_atom(ident.value),
                        _ => return None,
                    };
                    (class_atom, method_name)
                }
                _ => return None,
            };

            Some(ReturnExpressionHint::StaticMethodCall {
                class: class_name,
                method: method_name,
            })
        }
        Expression::Call(Call::Function(function_call)) => {
            // Handle: return someFunction(...);
            let function_name = match &function_call.function {
                Expression::Identifier(Identifier::Local(local)) => {
                    let resolved_name = context.resolved_names.get(&local.span);
                    let stripped = if resolved_name.starts_with('\\') { &resolved_name[1..] } else { resolved_name };
                    ascii_lowercase_atom(stripped)
                }
                Expression::Identifier(Identifier::Qualified(qualified)) => {
                    let value = qualified.value;
                    let stripped = if value.starts_with('\\') { &value[1..] } else { value };
                    ascii_lowercase_atom(stripped)
                }
                Expression::Identifier(Identifier::FullyQualified(fully_qualified)) => {
                    let value = fully_qualified.value;
                    let stripped = if value.starts_with('\\') { &value[1..] } else { value };
                    ascii_lowercase_atom(stripped)
                }
                _ => return None,
            };

            Some(ReturnExpressionHint::FunctionCall {
                function: function_name,
            })
        }
        _ => None,
    }
}

/// Extracts a method chain from an expression, returning (starting_class, [methods]).
/// For example, from `$this->a()->b()` returns (CurrentClass, ["a", "b"])
fn extract_method_chain_from_expression(
    expr: &Expression<'_>,
    current_classname: Option<Atom>,
    context: &Context<'_, '_>,
) -> Option<(Atom, Vec<Atom>)> {
    match expr {
        Expression::Call(Call::Method(method_call)) => {
            // Recursively extract from the receiver
            if let Some((receiver_class, mut methods)) = extract_method_chain_from_expression(&method_call.object, current_classname, context) {
                let method_name = match &method_call.method {
                    ClassLikeMemberSelector::Identifier(ident) => ascii_lowercase_atom(ident.value),
                    _ => return None,
                };
                methods.push(method_name);
                return Some((receiver_class, methods));
            }
            None
        }
        Expression::Call(Call::StaticMethod(static_call)) => {
            // Static method call starts a chain
            let (class_name, method_name) = match &static_call.class {
                // static::method() - parsed as Expression::Static
                Expression::Static(_) => {
                    (current_classname?, extract_method_name(&static_call.method)?)
                }
                // self::method() - parsed as Expression::Self_
                Expression::Self_(_) => {
                    (current_classname?, extract_method_name(&static_call.method)?)
                }
                // parent::method()
                Expression::Parent(_) => {
                    (current_classname?, extract_method_name(&static_call.method)?)
                }
                Expression::Identifier(Identifier::Local(local)) => {
                    let resolved_name = context.resolved_names.get(&local.span);
                    (ascii_lowercase_atom(resolved_name), extract_method_name(&static_call.method)?)
                }
                Expression::Identifier(Identifier::Qualified(qualified)) => {
                    (ascii_lowercase_atom(qualified.value), extract_method_name(&static_call.method)?)
                }
                Expression::Identifier(Identifier::FullyQualified(fully_qualified)) => {
                    let value = fully_qualified.value;
                    let stripped = if value.starts_with('\\') { &value[1..] } else { value };
                    (ascii_lowercase_atom(stripped), extract_method_name(&static_call.method)?)
                }
                _ => return None,
            };
            Some((class_name, vec![method_name]))
        }
        Expression::Variable(Variable::Direct(direct)) if direct.name.eq_ignore_ascii_case("$this") => {
            // $this is the receiver
            Some((current_classname?, vec![]))
        }
        _ => None,
    }
}

fn extract_method_name(selector: &ClassLikeMemberSelector) -> Option<Atom> {
    match selector {
        ClassLikeMemberSelector::Identifier(ident) => Some(ascii_lowercase_atom(ident.value)),
        _ => None,
    }
}

fn argument_value<'a, 'arena>(arg: &'a Argument<'arena>) -> &'a Expression<'arena> {
    match arg {
        Argument::Positional(p) => p.value,
        Argument::Named(n) => n.value,
    }
}

/// Check if a block contains calls to `func_get_args()`, `func_get_arg()`, or `func_num_args()`.
/// These functions indicate the method implicitly accepts variadic arguments.
fn block_has_func_get_args(block: &Block) -> bool {
    for statement in &block.statements {
        if statement_has_func_get_args(statement) {
            return true;
        }
    }
    false
}

fn statement_has_func_get_args(statement: &Statement) -> bool {
    match statement {
        Statement::Expression(expr_stmt) => expression_has_func_get_args(expr_stmt.expression),
        Statement::Return(ret) => ret.value.as_ref().is_some_and(|e| expression_has_func_get_args(e)),
        Statement::Block(block) => block_has_func_get_args(block),
        Statement::If(r#if) => match &r#if.body {
            mago_syntax::ast::IfBody::Statement(body) => {
                statement_has_func_get_args(body.statement)
                    || body.else_if_clauses.iter().any(|c| statement_has_func_get_args(c.statement))
                    || body.else_clause.as_ref().is_some_and(|c| statement_has_func_get_args(c.statement))
            }
            mago_syntax::ast::IfBody::ColonDelimited(body) => {
                body.statements.iter().any(statement_has_func_get_args)
                    || body.else_if_clauses.iter().any(|c| c.statements.iter().any(statement_has_func_get_args))
                    || body.else_clause.as_ref().is_some_and(|c| c.statements.iter().any(statement_has_func_get_args))
            }
        },
        Statement::Try(r#try) => {
            r#try.block.statements.iter().any(statement_has_func_get_args)
                || r#try.catch_clauses.iter().any(|c| block_has_func_get_args(&c.block))
                || r#try.finally_clause.as_ref().is_some_and(|f| block_has_func_get_args(&f.block))
        }
        Statement::Foreach(foreach) => match &foreach.body {
            mago_syntax::ast::ForeachBody::Statement(s) => statement_has_func_get_args(s),
            mago_syntax::ast::ForeachBody::ColonDelimited(b) => b.statements.iter().any(statement_has_func_get_args),
        },
        _ => false,
    }
}

fn expression_has_func_get_args(expression: &Expression) -> bool {
    match expression {
        Expression::Call(Call::Function(func_call)) => {
            if let Expression::Identifier(ident) = func_call.function {
                let name = ident.value();
                if name.eq_ignore_ascii_case("func_get_args")
                    || name.eq_ignore_ascii_case("func_get_arg")
                    || name.eq_ignore_ascii_case("func_num_args")
                {
                    return true;
                }
            }
            // Check arguments recursively
            func_call.argument_list.arguments.iter().any(|arg| {
                expression_has_func_get_args(argument_value(arg))
            })
        }
        Expression::Call(Call::Method(method_call)) => {
            method_call.argument_list.arguments.iter().any(|arg| {
                expression_has_func_get_args(argument_value(arg))
            })
        }
        Expression::Call(Call::StaticMethod(static_call)) => {
            static_call.argument_list.arguments.iter().any(|arg| {
                expression_has_func_get_args(argument_value(arg))
            })
        }
        Expression::Assignment(assignment) => {
            expression_has_func_get_args(assignment.lhs) || expression_has_func_get_args(assignment.rhs)
        }
        Expression::Parenthesized(p) => expression_has_func_get_args(p.expression),
        Expression::Binary(op) => expression_has_func_get_args(op.lhs) || expression_has_func_get_args(op.rhs),
        Expression::UnaryPrefix(op) => expression_has_func_get_args(op.operand),
        Expression::Conditional(cond) => {
            expression_has_func_get_args(cond.condition)
                || cond.then.as_ref().is_some_and(|e| expression_has_func_get_args(e))
                || expression_has_func_get_args(cond.r#else)
        }
        _ => false,
    }
}
