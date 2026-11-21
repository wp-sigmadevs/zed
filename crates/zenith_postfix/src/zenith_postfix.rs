mod choice_picker;
mod editor_actions;
pub mod live_templates;
pub use editor_actions::init;

use std::path::Path;
use serde::{Deserialize, Serialize};

gpui::actions!(
    zenith_postfix,
    [
        ExpandPostfix,
        SurroundWith,
        LiveTemplates,
    ]
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
        if stripped.ends_with("ch") || stripped.ends_with("sh") || stripped.ends_with('x') || stripped.ends_with('s') {
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
            SurroundKind::While => format!("{indent}while ($condition) {{\n{indented_code}\n{indent}}}"),
            SurroundKind::Function => format!("{indent}function wrapper() {{\n{indented_code}\n{indent}}}"),
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
            SurroundKind::While => format!("{indent}while (condition) {{\n{indented_code}\n{indent}}}"),
            SurroundKind::Function => format!("{indent}function wrapper() {{\n{indented_code}\n{indent}}}"),
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
        assert_eq!(res2.replacement, "foreach ($categories as $category) {\n    \n}");

        let res3 = expand_postfix("$this->getItems().foreach", LanguageKind::Php).expect("expand");
        assert_eq!(res3.replacement, "foreach ($this->getItems() as $item) {\n    \n}");
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
        assert_eq!(forof_res.replacement, "for (const order of orders) {\n  \n}");

        let const_res = expand_postfix("calculateDiscount().const", LanguageKind::TypeScript).unwrap();
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
        let surrounded_js = surround_with(js_code, SurroundKind::TryCatch, LanguageKind::TypeScript, "  ");
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
}
