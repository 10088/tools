use super::{
    AnalyzerCapabilities, DebugCapabilities, ExtensionHandler, FormatterCapabilities, LintParams,
    LintResults, Mime, ParserCapabilities,
};
use crate::configuration::to_analyzer_configuration;
use crate::file_handlers::{FixAllParams, Language as LanguageId};
use crate::{
    settings::{FormatSettings, Language, LanguageSettings, LanguagesSettings, SettingsHandle},
    workspace::{
        CodeAction, FixAction, FixFileMode, FixFileResult, GetSyntaxTreeResult, PullActionsResult,
        RenameResult,
    },
    Rules, WorkspaceError,
};
use indexmap::IndexSet;
use rome_analyze::{
    AnalysisFilter, AnalyzerOptions, ControlFlow, GroupCategory, Never, QueryMatch,
    RegistryVisitor, RuleCategories, RuleCategory, RuleFilter, RuleGroup,
};
use rome_diagnostics::{category, Applicability, Diagnostic, DiagnosticExt, Severity};
use rome_formatter::{FormatError, Printed};
use rome_fs::RomePath;
use rome_js_analyze::utils::rename::{RenameError, RenameSymbolExtensions};
use rome_js_analyze::{
    analyze, analyze_with_inspect_matcher, visit_registry, ControlFlowGraph, RuleError,
};
use rome_js_formatter::context::{
    trailing_comma::TrailingComma, QuoteProperties, QuoteStyle, Semicolons,
};
use rome_js_formatter::{context::JsFormatOptions, format_node};
use rome_js_semantic::{semantic_model, SemanticModelOptions};
use rome_js_syntax::{
    AnyJsRoot, JsLanguage, JsSyntaxNode, SourceType, TextRange, TextSize, TokenAtOffset,
};
use rome_parser::AnyParse;
use rome_rowan::{AstNode, BatchMutationExt, Direction};
use std::borrow::Cow;
use std::fmt::Debug;
use tracing::debug;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct JsFormatterSettings {
    pub quote_style: Option<QuoteStyle>,
    pub quote_properties: Option<QuoteProperties>,
    pub trailing_comma: Option<TrailingComma>,
    pub semicolons: Option<Semicolons>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
pub struct JsLinterSettings {
    pub globals: Vec<String>,
}

impl Language for JsLanguage {
    type FormatterSettings = JsFormatterSettings;
    type LinterSettings = JsLinterSettings;
    type FormatOptions = JsFormatOptions;

    fn lookup_settings(languages: &LanguagesSettings) -> &LanguageSettings<Self> {
        &languages.javascript
    }

    fn resolve_format_options(
        global: &FormatSettings,
        language: &JsFormatterSettings,
        path: &RomePath,
    ) -> JsFormatOptions {
        JsFormatOptions::new(path.as_path().try_into().unwrap_or_default())
            .with_indent_style(global.indent_style.unwrap_or_default())
            .with_line_width(global.line_width.unwrap_or_default())
            .with_quote_style(language.quote_style.unwrap_or_default())
            .with_quote_properties(language.quote_properties.unwrap_or_default())
            .with_trailing_comma(language.trailing_comma.unwrap_or_default())
            .with_semicolons(language.semicolons.unwrap_or_default())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct JsFileHandler;

impl ExtensionHandler for JsFileHandler {
    fn language(&self) -> super::Language {
        super::Language::JavaScript
    }

    fn mime(&self) -> Mime {
        Mime::Javascript
    }

    fn may_use_tabs(&self) -> bool {
        true
    }

    fn capabilities(&self) -> super::Capabilities {
        super::Capabilities {
            parser: ParserCapabilities { parse: Some(parse) },
            debug: DebugCapabilities {
                debug_syntax_tree: Some(debug_syntax_tree),
                debug_control_flow: Some(debug_control_flow),
                debug_formatter_ir: Some(debug_formatter_ir),
            },
            analyzer: AnalyzerCapabilities {
                lint: Some(lint),
                code_actions: Some(code_actions),
                fix_all: Some(fix_all),
                rename: Some(rename),
            },
            formatter: FormatterCapabilities {
                format: Some(format),
                format_range: Some(format_range),
                format_on_type: Some(format_on_type),
            },
        }
    }
}

fn parse(rome_path: &RomePath, language_hint: LanguageId, text: &str) -> AnyParse {
    let source_type =
        SourceType::try_from(rome_path.as_path()).unwrap_or_else(|_| match language_hint {
            LanguageId::JavaScriptReact => SourceType::jsx(),
            LanguageId::TypeScript => SourceType::ts(),
            LanguageId::TypeScriptReact => SourceType::tsx(),
            _ => SourceType::js_module(),
        });

    let parse = rome_js_parser::parse(text, source_type);
    AnyParse::from(parse)
}

fn debug_syntax_tree(_rome_path: &RomePath, parse: AnyParse) -> GetSyntaxTreeResult {
    let syntax: JsSyntaxNode = parse.syntax();
    let tree: AnyJsRoot = parse.tree();
    GetSyntaxTreeResult {
        cst: format!("{syntax:#?}"),
        ast: format!("{tree:#?}"),
    }
}

fn debug_control_flow(parse: AnyParse, cursor: TextSize) -> String {
    let mut control_flow_graph = None;

    let filter = AnalysisFilter {
        categories: RuleCategories::LINT,
        enabled_rules: Some(&[RuleFilter::Rule("correctness", "noUnreachable")]),
        ..AnalysisFilter::default()
    };
    let options = AnalyzerOptions::default();

    analyze_with_inspect_matcher(
        &parse.tree(),
        filter,
        |match_params| {
            let cfg = match match_params.query.downcast_ref::<ControlFlowGraph>() {
                Some(cfg) => cfg,
                _ => return,
            };

            let range = cfg.text_range();
            if !range.contains(cursor) {
                return;
            }

            match &control_flow_graph {
                None => {
                    control_flow_graph = Some((cfg.graph.to_string(), range));
                }
                Some((_, prev_range)) => {
                    if range.len() < prev_range.len() {
                        control_flow_graph = Some((cfg.graph.to_string(), range));
                    }
                }
            }
        },
        &options,
        |_| ControlFlow::<Never>::Continue(()),
    );

    control_flow_graph.map(|(cfg, _)| cfg).unwrap_or_default()
}

fn debug_formatter_ir(
    rome_path: &RomePath,
    parse: AnyParse,
    settings: SettingsHandle,
) -> Result<String, WorkspaceError> {
    let options = settings.format_options::<JsLanguage>(rome_path);

    let tree = parse.syntax();
    let formatted = format_node(options, &tree)?;

    let root_element = formatted.into_document();
    Ok(root_element.to_string())
}

fn lint(params: LintParams) -> LintResults {
    let tree = params.parse.tree();
    let mut diagnostics = params.parse.into_diagnostics();

    let analyzer_options = compute_analyzer_options(&params.settings);

    let mut diagnostic_count = diagnostics.len() as u64;
    let mut errors = diagnostics
        .iter()
        .filter(|diag| diag.severity() <= Severity::Error)
        .count();

    let has_lint = params.filter.categories.contains(RuleCategories::LINT);

    let (_, analyze_diagnostics) = analyze(&tree, params.filter, &analyzer_options, |signal| {
        if let Some(mut diagnostic) = signal.diagnostic() {
            // Do not report unused suppression comment diagnostics if this is a syntax-only analyzer pass
            if !has_lint && diagnostic.category() == Some(category!("suppressions/unused")) {
                return ControlFlow::<Never>::Continue(());
            }

            diagnostic_count += 1;

            // We do now check if the severity of the diagnostics should be changed.
            // The configuration allows to change the severity of the diagnostics emitted by rules.
            let severity = diagnostic
                .category()
                .filter(|category| category.name().starts_with("lint/"))
                .map(|category| {
                    params
                        .rules
                        .and_then(|rules| rules.get_severity_from_code(category))
                        .unwrap_or(Severity::Warning)
                })
                .unwrap_or_else(|| diagnostic.severity());

            if severity <= Severity::Error {
                errors += 1;
            }

            if diagnostic_count <= params.max_diagnostics {
                for action in signal.actions() {
                    if !action.is_suppression() {
                        diagnostic = diagnostic.add_code_suggestion(action.into());
                    }
                }

                let error = diagnostic.with_severity(severity);

                diagnostics.push(rome_diagnostics::serde::Diagnostic::new(error));
            }
        }

        ControlFlow::<Never>::Continue(())
    });

    diagnostics.extend(
        analyze_diagnostics
            .into_iter()
            .map(rome_diagnostics::serde::Diagnostic::new)
            .collect::<Vec<_>>(),
    );
    let skipped_diagnostics = diagnostic_count - diagnostics.len() as u64;

    LintResults {
        diagnostics,
        errors,
        skipped_diagnostics,
    }
}

struct ActionsVisitor<'a> {
    enabled_rules: Vec<RuleFilter<'a>>,
}

impl RegistryVisitor<JsLanguage> for ActionsVisitor<'_> {
    fn record_category<C: GroupCategory<Language = JsLanguage>>(&mut self) {
        if matches!(C::CATEGORY, RuleCategory::Action) {
            C::record_groups(self);
        }
    }

    fn record_group<G: RuleGroup<Language = JsLanguage>>(&mut self) {
        G::record_rules(self)
    }

    fn record_rule<R>(&mut self)
    where
        R: rome_analyze::Rule + 'static,
        R::Query: rome_analyze::Queryable<Language = JsLanguage>,
        <R::Query as rome_analyze::Queryable>::Output: Clone,
    {
        self.enabled_rules.push(RuleFilter::Rule(
            <R::Group as RuleGroup>::NAME,
            R::METADATA.name,
        ));
    }
}

fn code_actions(
    parse: AnyParse,
    range: TextRange,
    rules: Option<&Rules>,
    settings: SettingsHandle,
) -> PullActionsResult {
    let tree = parse.tree();

    let mut actions = Vec::new();

    let enabled_rules: Option<Vec<RuleFilter>> = if let Some(rules) = rules {
        let enabled_rules = rules.as_enabled_rules().into_iter().collect();

        // The rules in the assist category do not have configuration entries,
        // always add them all to the enabled rules list
        let mut visitor = ActionsVisitor { enabled_rules };
        visit_registry(&mut visitor);

        Some(visitor.enabled_rules)
    } else {
        None
    };

    let mut filter = match &enabled_rules {
        Some(rules) => AnalysisFilter::from_enabled_rules(Some(rules.as_slice())),
        _ => AnalysisFilter::default(),
    };

    filter.categories = RuleCategories::default();
    filter.range = Some(range);

    let analyzer_options = compute_analyzer_options(&settings);

    analyze(&tree, filter, &analyzer_options, |signal| {
        actions.extend(signal.actions().into_code_action_iter().map(|item| {
            CodeAction {
                category: item.category.clone(),
                rule_name: item
                    .rule_name
                    .map(|(group, name)| (Cow::Borrowed(group), Cow::Borrowed(name))),
                suggestion: item.suggestion,
            }
        }));

        ControlFlow::<Never>::Continue(())
    });

    PullActionsResult { actions }
}

/// If applies all the safe fixes to the given syntax tree.
///
/// If `indent_style` is [Some], it means that the formatting should be applied at the end
fn fix_all(params: FixAllParams) -> Result<FixFileResult, WorkspaceError> {
    let FixAllParams {
        parse,
        rules,
        fix_file_mode,
        settings,
    } = params;

    let mut tree: AnyJsRoot = parse.tree();
    let mut actions = Vec::new();

    let enabled_rules: Option<Vec<RuleFilter>> = if let Some(rules) = rules {
        let enabled: IndexSet<RuleFilter> = rules.as_enabled_rules();
        Some(enabled.into_iter().collect())
    } else {
        None
    };

    let mut filter = match &enabled_rules {
        Some(rules) => AnalysisFilter::from_enabled_rules(Some(rules.as_slice())),
        _ => AnalysisFilter::default(),
    };

    filter.categories = RuleCategories::SYNTAX | RuleCategories::LINT;
    let mut skipped_suggested_fixes = 0;
    let analyzer_options = compute_analyzer_options(&settings);
    loop {
        let (action, _) = analyze(&tree, filter, &analyzer_options, |signal| {
            for action in signal.actions() {
                // suppression actions should not be part of the fixes (safe or suggested)
                if action.is_suppression() {
                    continue;
                }
                match fix_file_mode {
                    FixFileMode::SafeFixes => {
                        if action.applicability == Applicability::MaybeIncorrect {
                            skipped_suggested_fixes += 1;
                        }
                        if action.applicability == Applicability::Always {
                            return ControlFlow::Break(action);
                        }
                    }
                    FixFileMode::SafeAndSuggestedFixes => {
                        if matches!(
                            action.applicability,
                            Applicability::Always | Applicability::MaybeIncorrect
                        ) {
                            return ControlFlow::Break(action);
                        }
                    }
                }
            }

            ControlFlow::Continue(())
        });

        match action {
            Some(action) => {
                if let Some((range, _)) = action.mutation.as_text_edits() {
                    tree = match AnyJsRoot::cast(action.mutation.commit()) {
                        Some(tree) => tree,
                        None => {
                            return Err(WorkspaceError::RuleError(
                                RuleError::ReplacedRootWithNonRootError {
                                    rule_name: action.rule_name.map(|(group, rule)| {
                                        (Cow::Borrowed(group), Cow::Borrowed(rule))
                                    }),
                                },
                            ))
                        }
                    };
                    actions.push(FixAction {
                        rule_name: action
                            .rule_name
                            .map(|(group, rule)| (Cow::Borrowed(group), Cow::Borrowed(rule))),
                        range,
                    });
                }
            }
            None => {
                return Ok(FixFileResult {
                    code: tree.syntax().to_string(),
                    skipped_suggested_fixes,
                    actions,
                });
            }
        }
    }
}

#[tracing::instrument(level = "debug", skip(parse))]
fn format(
    rome_path: &RomePath,
    parse: AnyParse,
    settings: SettingsHandle,
) -> Result<Printed, WorkspaceError> {
    let options = settings.format_options::<JsLanguage>(rome_path);

    debug!("Format with the following options: \n{}", options);

    let tree = parse.syntax();
    let formatted = format_node(options, &tree)?;

    match formatted.print() {
        Ok(printed) => Ok(printed),
        Err(error) => Err(WorkspaceError::FormatError(error.into())),
    }
}

fn format_range(
    rome_path: &RomePath,
    parse: AnyParse,
    settings: SettingsHandle,
    range: TextRange,
) -> Result<Printed, WorkspaceError> {
    let options = settings.format_options::<JsLanguage>(rome_path);

    let tree = parse.syntax();
    let printed = rome_js_formatter::format_range(options, &tree, range)?;
    Ok(printed)
}

fn format_on_type(
    rome_path: &RomePath,
    parse: AnyParse,
    settings: SettingsHandle,
    offset: TextSize,
) -> Result<Printed, WorkspaceError> {
    let options = settings.format_options::<JsLanguage>(rome_path);

    let tree = parse.syntax();

    let range = tree.text_range();
    if offset < range.start() || offset > range.end() {
        return Err(WorkspaceError::FormatError(FormatError::RangeError {
            input: TextRange::at(offset, TextSize::from(0)),
            tree: range,
        }));
    }

    let token = match tree.token_at_offset(offset) {
        // File is empty, do nothing
        TokenAtOffset::None => panic!("empty file"),
        TokenAtOffset::Single(token) => token,
        // The cursor should be right after the closing character that was just typed,
        // select the previous token as the correct one
        TokenAtOffset::Between(token, _) => token,
    };

    let root_node = match token.parent() {
        Some(node) => node,
        None => panic!("found a token with no parent"),
    };

    let printed = rome_js_formatter::format_sub_tree(options, &root_node)?;
    Ok(printed)
}

fn rename(
    _rome_path: &RomePath,
    parse: AnyParse,
    symbol_at: TextSize,
    new_name: String,
) -> Result<RenameResult, WorkspaceError> {
    let root = parse.tree();
    let model = semantic_model(&root, SemanticModelOptions::default());

    if let Some(node) = parse
        .syntax()
        .descendants_tokens(Direction::Next)
        .find(|token| token.text_range().contains(symbol_at))
        .and_then(|token| token.parent())
    {
        let original_name = node.text_trimmed();
        let range = node.text_range();
        match node.try_into() {
            Ok(node) => {
                let mut batch = root.begin();
                let result = batch.rename_any_renamable_node(&model, node, &new_name);
                if !result {
                    Err(WorkspaceError::RenameError(RenameError::CannotBeRenamed {
                        original_name: original_name.to_string(),
                        original_range: range,
                        new_name,
                    }))
                } else {
                    let (range, indels) = batch.as_text_edits().unwrap_or_default();
                    Ok(RenameResult { range, indels })
                }
            }
            Err(err) => Err(WorkspaceError::RenameError(err)),
        }
    } else {
        Err(WorkspaceError::RenameError(
            RenameError::CannotFindDeclaration(new_name),
        ))
    }
}

fn compute_analyzer_options(settings: &SettingsHandle) -> AnalyzerOptions {
    let configuration = to_analyzer_configuration(
        settings.as_ref().linter(),
        &settings.as_ref().languages,
        |settings| {
            if let Some(globals) = settings.javascript.globals.as_ref() {
                globals
                    .iter()
                    .map(|global| global.to_string())
                    .collect::<Vec<_>>()
            } else {
                vec![]
            }
        },
    );
    AnalyzerOptions { configuration }
}
