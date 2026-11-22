mod choice_picker;
mod editor_actions;
pub mod live_templates;
pub use editor_actions::init;

use serde::{Deserialize, Serialize};
use std::path::Path;

gpui::actions!(
    zenith_postfix,
    [ExpandPostfix, SurroundWith, LiveTemplates,]
);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LanguageKind {
    Php,
    JavaScript,
    TypeScript,
    React,
}

impl LanguageKind {
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_lowercase();
        Self::from_extension(&ext)
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "php" | "phtml" => Some(Self::Php),
            "js" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "mts" | "cts" => Some(Self::TypeScript),
            "jsx" | "tsx" => Some(Self::React),
            _ => None,
        }
    }

    pub fn is_php(&self) -> bool {
        matches!(self, Self::Php)
    }

    pub fn is_js_or_ts(&self) -> bool {
        matches!(self, Self::JavaScript | Self::TypeScript | Self::React)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostfixTemplate {
    pub key: String,
    pub description: String,
    pub example: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostfixResult {
    pub expression: String,
    pub trigger: String,
    pub replacement: String,
    pub cursor_offset: Option<usize>,
}

pub fn get_available_postfix_templates(lang: LanguageKind) -> Vec<PostfixTemplate> {
    if lang.is_php() {
        vec![
            PostfixTemplate {
                key: "if".into(),
                description: "if ($expr) { ... }".into(),
                example: "$user.if".into(),
            },
            PostfixTemplate {
                key: "not".into(),
                description: "!$expr".into(),
                example: "$valid.not".into(),
            },
            PostfixTemplate {
                key: "notnull".into(),
                description: "if ($expr !== null) { ... }".into(),
                example: "$account.notnull".into(),
            },
            PostfixTemplate {
                key: "null".into(),
                description: "if ($expr === null) { ... }".into(),
                example: "$account.null".into(),
            },
            PostfixTemplate {
                key: "foreach".into(),
                description: "foreach ($expr as $item) { ... }".into(),
                example: "$items.foreach".into(),
            },
            PostfixTemplate {
                key: "return".into(),
                description: "return $expr;".into(),
                example: "$result.return".into(),
            },
            PostfixTemplate {
                key: "var".into(),
                description: "$var = $expr;".into(),
                example: "$query.var".into(),
            },
            PostfixTemplate {
                key: "isset".into(),
                description: "isset($expr)".into(),
                example: "$data['key'].isset".into(),
            },
            PostfixTemplate {
                key: "empty".into(),
                description: "empty($expr)".into(),
                example: "$items.empty".into(),
            },
            PostfixTemplate {
                key: "throw".into(),
                description: "throw $expr;".into(),
                example: "new Exception('error').throw".into(),
            },
            PostfixTemplate {
                key: "par".into(),
                description: "($expr)".into(),
                example: "$a + $b.par".into(),
            },
            PostfixTemplate {
                key: "echo".into(),
                description: "echo $expr;".into(),
                example: "$message.echo".into(),
            },
            PostfixTemplate {
                key: "dd".into(),
                description: "dd($expr);".into(),
                example: "$user.dd".into(),
            },
            PostfixTemplate {
                key: "dump".into(),
                description: "dump($expr);".into(),
                example: "$user.dump".into(),
            },
        ]
    } else {
        vec![
            PostfixTemplate {
                key: "if".into(),
                description: "if (expr) { ... }".into(),
                example: "isValid.if".into(),
            },
            PostfixTemplate {
                key: "not".into(),
                description: "!expr".into(),
                example: "isValid.not".into(),
            },
            PostfixTemplate {
                key: "const".into(),
                description: "const name = expr;".into(),
                example: "calculate().const".into(),
            },
            PostfixTemplate {
                key: "let".into(),
                description: "let name = expr;".into(),
                example: "calculate().let".into(),
            },
            PostfixTemplate {
                key: "return".into(),
                description: "return expr;".into(),
                example: "result.return".into(),
            },
            PostfixTemplate {
                key: "log".into(),
                description: "console.log(expr);".into(),
                example: "data.log".into(),
            },
            PostfixTemplate {
                key: "await".into(),
                description: "await expr".into(),
                example: "fetchData().await".into(),
            },
            PostfixTemplate {
                key: "forof".into(),
                description: "for (const item of expr) { ... }".into(),
                example: "items.forof".into(),
            },
            PostfixTemplate {
                key: "par".into(),
                description: "(expr)".into(),
                example: "a + b.par".into(),
            },
        ]
    }
}

pub fn deduce_singular(plural: &str) -> String {
    let clean = plural.trim_start_matches('$');
    let singular_clean = if let Some(stripped) = clean.strip_suffix("ies") {
        format!("{stripped}y")
    } else if let Some(stripped) = clean.strip_suffix("es") {
        if stripped.ends_with("ch")
            || stripped.ends_with("sh")
            || stripped.ends_with('x')
            || stripped.ends_with('s')
        {
            stripped.to_string()
        } else {
            format!("{stripped}e")
        }
    } else if let Some(stripped) = clean.strip_suffix('s') {
        if stripped.ends_with('s') {
            clean.to_string()
        } else {
            stripped.to_string()
        }
    } else {
        "item".to_string()
    };

    if plural.starts_with('$') {
        format!("${singular_clean}")
    } else {
        singular_clean
    }
}

pub fn extract_expression_and_trigger(line_prefix: &str) -> Option<(String, String)> {
    let trimmed = line_prefix.trim_end();
    let dot_pos = trimmed.rfind('.')?;
    let trigger = &trimmed[dot_pos + 1..];
    if trigger.is_empty() || trigger.contains(|c: char| !c.is_alphanumeric() && c != '_') {
        return None;
    }

    let before_dot = &trimmed[..dot_pos];
    if before_dot.is_empty() {
        return None;
    }

    let expr = parse_preceding_expression(before_dot)?;
    Some((expr, trigger.to_string()))
}

fn parse_preceding_expression(text: &str) -> Option<String> {
    let mut paren_depth = 0;
    let mut bracket_depth = 0;
    let mut brace_depth = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;

    let chars: Vec<char> = text.chars().collect();
    let mut start_idx = 0;

    for i in (0..chars.len()).rev() {
        let c = chars[i];

        if c == '\'' && !in_double_quote {
            in_single_quote = !in_single_quote;
            continue;
        }
        if c == '"' && !in_single_quote {
            in_double_quote = !in_double_quote;
            continue;
        }

        if in_single_quote || in_double_quote {
            continue;
        }

        match c {
            ')' => paren_depth += 1,
            '(' => {
                if paren_depth > 0 {
                    paren_depth -= 1;
                } else {
                    start_idx = i + 1;
                    break;
                }
            }
            ']' => bracket_depth += 1,
            '[' => {
                if bracket_depth > 0 {
                    bracket_depth -= 1;
                } else {
                    start_idx = i + 1;
                    break;
                }
            }
            '}' => brace_depth += 1,
            '{' => {
                if brace_depth > 0 {
                    brace_depth -= 1;
                } else {
                    start_idx = i + 1;
                    break;
                }
            }
            ';' | ',' | '=' if paren_depth == 0 && bracket_depth == 0 && brace_depth == 0 => {
                start_idx = i + 1;
                break;
            }
            _ => {}
        }
    }

    let candidate = chars[start_idx..].iter().collect::<String>();
    let trimmed = candidate.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

pub fn expand_postfix(line_prefix: &str, lang: LanguageKind) -> Option<PostfixResult> {
    let (expr, trigger) = extract_expression_and_trigger(line_prefix)?;

    let replacement = if lang.is_php() {
        match trigger.as_str() {
            "if" => format!("if ({expr}) {{\n    \n}}"),
            "not" => format!("!{expr}"),
            "notnull" => format!("if ({expr} !== null) {{\n    \n}}"),
            "null" => format!("if ({expr} === null) {{\n    \n}}"),
            "foreach" => {
                let item = deduce_singular(&expr);
                format!("foreach ({expr} as {item}) {{\n    \n}}")
            }
            "return" => format!("return {expr};"),
            "var" => {
                let var_name = deduce_singular(&expr);
                format!("{var_name} = {expr};")
            }
            "isset" => format!("isset({expr})"),
            "empty" => format!("empty({expr})"),
            "throw" => {
                if expr.starts_with("new ") {
                    format!("throw {expr};")
                } else {
                    format!("throw new \\Exception({expr});")
                }
            }
            "par" => format!("({expr})"),
            "echo" => format!("echo {expr};"),
            "dd" => format!("dd({expr});"),
            "dump" => format!("dump({expr});"),
            _ => return None,
        }
    } else {
        match trigger.as_str() {
            "if" => format!("if ({expr}) {{\n  \n}}"),
            "not" => format!("!{expr}"),
            "const" => {
                let var_name = deduce_singular(&expr).trim_start_matches('$').to_string();
                format!("const {var_name} = {expr};")
            }
            "let" => {
                let var_name = deduce_singular(&expr).trim_start_matches('$').to_string();
                format!("let {var_name} = {expr};")
            }
            "return" => format!("return {expr};"),
            "log" => format!("console.log({expr});"),
            "await" => format!("await {expr}"),
            "forof" => {
                let item = deduce_singular(&expr).trim_start_matches('$').to_string();
                format!("for (const {item} of {expr}) {{\n  \n}}")
            }
            "par" => format!("({expr})"),
            _ => return None,
        }
    };

    let cursor_offset = replacement.find("    \n").map(|pos| pos + 4);

    Some(PostfixResult {
        expression: expr,
        trigger,
        replacement,
        cursor_offset,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurroundKind {
    TryCatch,
    If,
    Foreach,
    While,
    Function,
}

pub fn surround_with(code: &str, kind: SurroundKind, lang: LanguageKind, indent: &str) -> String {
    let indented_code: String = code
        .lines()
        .map(|line| format!("{indent}    {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    if lang.is_php() {
        match kind {
            SurroundKind::TryCatch => format!(
                "{indent}try {{\n{indented_code}\n{indent}}} catch (\\Throwable $e) {{\n{indent}    // TODO: Handle exception\n{indent}}}"
            ),
            SurroundKind::If => format!("{indent}if ($condition) {{\n{indented_code}\n{indent}}}"),
            SurroundKind::Foreach => {
                format!("{indent}foreach ($items as $item) {{\n{indented_code}\n{indent}}}")
            }
            SurroundKind::While => {
                format!("{indent}while ($condition) {{\n{indented_code}\n{indent}}}")
            }
            SurroundKind::Function => {
                format!("{indent}function wrapper() {{\n{indented_code}\n{indent}}}")
            }
        }
    } else {
        match kind {
            SurroundKind::TryCatch => format!(
                "{indent}try {{\n{indented_code}\n{indent}}} catch (error) {{\n{indent}    console.error(error);\n{indent}}}"
            ),
            SurroundKind::If => format!("{indent}if (condition) {{\n{indented_code}\n{indent}}}"),
            SurroundKind::Foreach => {
                format!("{indent}for (const item of items) {{\n{indented_code}\n{indent}}}")
            }
            SurroundKind::While => {
                format!("{indent}while (condition) {{\n{indented_code}\n{indent}}}")
            }
            SurroundKind::Function => {
                format!("{indent}function wrapper() {{\n{indented_code}\n{indent}}}")
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct LiveTemplateContext {
    pub file_path: Option<String>,
    pub class_name: Option<String>,
    pub method_name: Option<String>,
    pub namespace: Option<String>,
    pub date: Option<String>,
    pub year: Option<String>,
}

pub fn expand_live_template(template: &str, ctx: &LiveTemplateContext) -> String {
    let mut result = template.to_string();

    if let Some(ref class) = ctx.class_name {
        result = result.replace("$CLASS_NAME$", class);
    }
    if let Some(ref method) = ctx.method_name {
        result = result.replace("$METHOD_NAME$", method);
    }
    if let Some(ref ns) = ctx.namespace {
        result = result.replace("$NAMESPACE$", ns);
    }
    if let Some(ref file) = ctx.file_path {
        result = result.replace("$FILE_NAME$", file);
    }
    if let Some(ref d) = ctx.date {
        result = result.replace("$DATE$", d);
    }
    if let Some(ref y) = ctx.year {
        result = result.replace("$YEAR$", y);
    }

    result.replace("$END$", "").replace("$CURSOR$", "")
}

impl LiveTemplateContext {
    /// The value of a built-in variable, or `None` when `name` is not built in or unknown here.
    fn builtin(&self, name: &str) -> Option<&str> {
        let value = match name {
            "CLASS_NAME" => &self.class_name,
            "METHOD_NAME" => &self.method_name,
            "NAMESPACE" => &self.namespace,
            "FILE_NAME" => &self.file_path,
            "DATE" => &self.date,
            "YEAR" => &self.year,
            _ => return None,
        };
        value.as_deref()
    }
}

enum TemplatePiece<'a> {
    Text(&'a str),
    End,
    Variable { name: &'a str, default: &'a str },
}

/// Splits a template body into literal text, `$END$` and `$NAME$` / `$NAME=default$` variables.
/// `$$` is a literal `$`, and any other `$` (PHP `$item`, `$_GET`) stays literal text.
// shortcut: names must be upper case so PHP variables are never read as template variables;
// an all-caps PHP variable directly followed by `$` or `=$` (`$A=$b`) needs `$$` to stay literal.
fn template_pieces(template: &str) -> Vec<TemplatePiece<'_>> {
    let mut pieces = Vec::new();
    let mut rest = template;
    while let Some(dollar) = rest.find('$') {
        pieces.push(TemplatePiece::Text(&rest[..dollar]));
        let after_dollar = &rest[dollar + 1..];
        if let Some(after) = after_dollar.strip_prefix('$') {
            pieces.push(TemplatePiece::Text("$"));
            rest = after;
            continue;
        }
        match parse_template_variable(after_dollar) {
            Some((name, default, after)) => {
                pieces.push(if name == "END" {
                    TemplatePiece::End
                } else {
                    TemplatePiece::Variable { name, default }
                });
                rest = after;
            }
            None => {
                pieces.push(TemplatePiece::Text("$"));
                rest = after_dollar;
            }
        }
    }
    pieces.push(TemplatePiece::Text(rest));
    pieces
}

/// Reads `NAME$` or `NAME=default$` after an opening `$`: (name, default, rest after the closing `$`).
fn parse_template_variable(source: &str) -> Option<(&str, &str, &str)> {
    if !source.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    let name_end = source
        .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .unwrap_or(source.len());
    let (name, after_name) = source.split_at(name_end);
    if let Some(rest) = after_name.strip_prefix('$') {
        return Some((name, "", rest));
    }
    let after_equals = after_name.strip_prefix('=')?;
    let (default, rest) = after_equals.split_once('$')?;
    (!default.contains('\n')).then_some((name, default, rest))
}

/// Escapes text so the snippet parser keeps it literal (`\`, `$` and `}` are special there).
fn push_snippet_literal(snippet: &mut String, text: &str) {
    for character in text.chars() {
        if matches!(character, '\\' | '$' | '}') {
            snippet.push('\\');
        }
        snippet.push(character);
    }
}

/// Converts a live template body into editor snippet syntax (`snippet::Snippet::parse`).
/// Built-in variables known in `ctx` become plain text; every other variable becomes a tab
/// stop, numbered by first appearance, holding its default (repeats share the stop, so they
/// mirror); `$END$` is the final stop `$0`.
pub fn template_to_snippet(template: &str, ctx: &LiveTemplateContext) -> String {
    let pieces = template_pieces(template);
    let mut stops: Vec<(&str, &str)> = Vec::new();
    for piece in &pieces {
        let TemplatePiece::Variable { name, default } = *piece else {
            continue;
        };
        if ctx.builtin(name).is_some() {
            continue;
        }
        // A repeat may carry the default the first occurrence left out.
        if let Some(stop) = stops.iter_mut().find(|stop| stop.0 == name) {
            if stop.1.is_empty() {
                stop.1 = default;
            }
        } else {
            stops.push((name, default));
        }
    }

    let mut snippet = String::with_capacity(template.len());
    for piece in pieces {
        match piece {
            TemplatePiece::Text(text) => push_snippet_literal(&mut snippet, text),
            TemplatePiece::End => snippet.push_str("$0"),
            TemplatePiece::Variable { name, .. } => {
                if let Some(value) = ctx.builtin(name) {
                    push_snippet_literal(&mut snippet, value);
                } else if let Some((index, (_, default))) =
                    stops.iter().enumerate().find(|(_, stop)| stop.0 == name)
                {
                    snippet.push_str(&format!("${{{}:", index + 1));
                    push_snippet_literal(&mut snippet, default);
                    snippet.push('}');
                }
            }
        }
    }
    snippet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_php_postfix_if_and_not() {
        let res = expand_postfix("let $check = $user->isActive().if", LanguageKind::Php)
            .expect("should expand");
        assert_eq!(res.expression, "$user->isActive()");
        assert_eq!(res.trigger, "if");
        assert_eq!(res.replacement, "if ($user->isActive()) {\n    \n}");

        let not_res = expand_postfix("$isValid.not", LanguageKind::Php).expect("should expand");
        assert_eq!(not_res.replacement, "!$isValid");
    }

    #[test]
    fn test_php_postfix_foreach_singular_deduction() {
        let res1 = expand_postfix("$users.foreach", LanguageKind::Php).expect("expand");
        assert_eq!(res1.replacement, "foreach ($users as $user) {\n    \n}");

        let res2 = expand_postfix("$categories.foreach", LanguageKind::Php).expect("expand");
        assert_eq!(
            res2.replacement,
            "foreach ($categories as $category) {\n    \n}"
        );

        let res3 = expand_postfix("$this->getItems().foreach", LanguageKind::Php).expect("expand");
        assert_eq!(
            res3.replacement,
            "foreach ($this->getItems() as $item) {\n    \n}"
        );
    }

    #[test]
    fn test_php_postfix_dd_and_dump_and_return() {
        let dd_res = expand_postfix("$order->calculateTotal().dd", LanguageKind::Php).unwrap();
        assert_eq!(dd_res.replacement, "dd($order->calculateTotal());");

        let dump_res = expand_postfix("$data.dump", LanguageKind::Php).unwrap();
        assert_eq!(dump_res.replacement, "dump($data);");

        let ret_res = expand_postfix("$this->user.return", LanguageKind::Php).unwrap();
        assert_eq!(ret_res.replacement, "return $this->user;");
    }

    #[test]
    fn test_js_postfix_log_await_forof() {
        let log_res = expand_postfix("response.data.log", LanguageKind::TypeScript).unwrap();
        assert_eq!(log_res.replacement, "console.log(response.data);");

        let await_res = expand_postfix("fetchUser(id).await", LanguageKind::TypeScript).unwrap();
        assert_eq!(await_res.replacement, "await fetchUser(id)");

        let forof_res = expand_postfix("orders.forof", LanguageKind::JavaScript).unwrap();
        assert_eq!(
            forof_res.replacement,
            "for (const order of orders) {\n  \n}"
        );

        let const_res =
            expand_postfix("calculateDiscount().const", LanguageKind::TypeScript).unwrap();
        assert_eq!(const_res.replacement, "const item = calculateDiscount();");
    }

    #[test]
    fn test_surround_with() {
        let code = "$user->save();\n$log->info('Saved');";
        let surrounded_php = surround_with(code, SurroundKind::TryCatch, LanguageKind::Php, "");
        assert!(surrounded_php.starts_with("try {"));
        assert!(surrounded_php.contains("$user->save();"));
        assert!(surrounded_php.contains("catch (\\Throwable $e)"));

        let js_code = "const res = await api();";
        let surrounded_js = surround_with(
            js_code,
            SurroundKind::TryCatch,
            LanguageKind::TypeScript,
            "  ",
        );
        assert!(surrounded_js.contains("  try {"));
        assert!(surrounded_js.contains("  } catch (error) {"));
    }

    #[test]
    fn test_live_template_expansion() {
        let template = "namespace $NAMESPACE$;\n\nclass $CLASS_NAME$ {\n    public function $METHOD_NAME$(): void {\n        $END$\n    }\n}";
        let ctx = LiveTemplateContext {
            namespace: Some("App\\Domain\\Billing".into()),
            class_name: Some("InvoiceCalculator".into()),
            method_name: Some("execute".into()),
            file_path: Some("InvoiceCalculator.php".into()),
            date: Some("2026-11-20".into()),
            year: Some("2026".into()),
        };

        let expanded = expand_live_template(template, &ctx);
        assert!(expanded.contains("namespace App\\Domain\\Billing;"));
        assert!(expanded.contains("class InvoiceCalculator"));
        assert!(expanded.contains("public function execute(): void"));
        assert!(!expanded.contains("$END$"));
    }

    fn parse(snippet: &str) -> snippet::Snippet {
        snippet::Snippet::parse(snippet).expect("the editor's snippet parser accepts it")
    }

    #[test]
    fn test_template_to_snippet_default_is_a_stop_and_end_is_final() {
        let snippet = template_to_snippet(
            "public function $NAME=handle$()\n{\n    $END$\n}",
            &LiveTemplateContext::default(),
        );
        assert_eq!(snippet, "public function ${1:handle}()\n{\n    $0\n\\}");
        let parsed = parse(&snippet);
        assert_eq!(parsed.text, "public function handle()\n{\n    \n}");
        assert_eq!(parsed.tabstops.len(), 2);
        assert_eq!(parsed.tabstops[0].ranges.as_slice(), &[16..22]);
        assert_eq!(parsed.tabstops[1].ranges.as_slice(), &[31..31]);
    }

    #[test]
    fn test_template_to_snippet_repeated_variable_mirrors_one_stop() {
        let snippet = template_to_snippet(
            "$TYPE$ $NAME=item$ = new $TYPE=Foo$();",
            &LiveTemplateContext::default(),
        );
        assert_eq!(snippet, "${1:Foo} ${2:item} = new ${1:Foo}();");
        let parsed = parse(&snippet);
        assert_eq!(parsed.text, "Foo item = new Foo();");
        assert_eq!(parsed.tabstops[0].ranges.as_slice(), &[0..3, 15..18]);
        assert_eq!(parsed.tabstops[1].ranges.as_slice(), &[4..8]);
    }

    #[test]
    fn test_template_to_snippet_keeps_php_dollars_literal() {
        let snippet = template_to_snippet(
            "foreach ($END$ as $item) {\n    \\Log::info($_GET, $$X$$, $A);\n}",
            &LiveTemplateContext::default(),
        );
        assert_eq!(
            snippet,
            "foreach ($0 as \\$item) {\n    \\\\Log::info(\\$_GET, \\$X\\$, \\$A);\n\\}"
        );
        let parsed = parse(&snippet);
        assert_eq!(
            parsed.text,
            "foreach ( as $item) {\n    \\Log::info($_GET, $X$, $A);\n}"
        );
        assert_eq!(parsed.tabstops.len(), 1, "only the final stop");
        assert_eq!(parsed.tabstops[0].ranges.as_slice(), &[9..9]);
    }

    #[test]
    fn test_template_to_snippet_fills_known_builtins_and_stops_on_unknown() {
        let ctx = LiveTemplateContext {
            class_name: Some("Invoice$}".into()),
            year: Some("2026".into()),
            ..Default::default()
        };
        let snippet = template_to_snippet(
            "// $YEAR$\nclass $CLASS_NAME$ { function $METHOD_NAME$() {} }",
            &ctx,
        );
        assert_eq!(
            snippet,
            "// 2026\nclass Invoice\\$\\} { function ${1:}() {\\} \\}"
        );
        let parsed = parse(&snippet);
        assert_eq!(parsed.text, "// 2026\nclass Invoice$} { function () {} }");
        assert_eq!(parsed.tabstops[0].ranges.as_slice(), &[35..35]);
    }

    #[test]
    fn test_builtin_live_templates_convert_to_valid_snippets() {
        for lang in [LanguageKind::Php, LanguageKind::TypeScript] {
            for template in live_templates::live_templates(lang) {
                let snippet = template_to_snippet(template.body, &LiveTemplateContext::default());
                let parsed = parse(&snippet);
                assert!(
                    !parsed.text.contains("$END$"),
                    "{}: {}",
                    template.abbreviation,
                    parsed.text
                );
            }
        }

        let php_foreach = live_templates::live_templates(LanguageKind::Php)
            .iter()
            .find(|template| template.abbreviation == "fore")
            .expect("PHP has a foreach template");
        let parsed = parse(&template_to_snippet(
            php_foreach.body,
            &LiveTemplateContext::default(),
        ));
        assert_eq!(parsed.text, "foreach ($items as $item) {\n    \n}");
        assert_eq!(parsed.tabstops.len(), 3);
        assert_eq!(parsed.tabstops[0].ranges.as_slice(), &[10..15]);
        assert_eq!(parsed.tabstops[1].ranges.as_slice(), &[20..24]);
    }
}
