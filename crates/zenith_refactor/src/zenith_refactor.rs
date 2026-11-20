use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

gpui::actions!(
    zenith_refactor,
    [
        RefactorThis,
        MoveClass,
        SafeDelete,
        ChangeSignature,
        ExtractMethod,
        ExtractVariable,
    ]
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RefactorKind {
    MoveClass {
        class_name: String,
        source_path: PathBuf,
        target_path: PathBuf,
        old_namespace: String,
        new_namespace: String,
    },
    SafeDelete {
        symbol_name: String,
        file_path: PathBuf,
    },
    ChangeSignature {
        function_name: String,
        file_path: PathBuf,
        reordered_indices: Vec<usize>,
        added_params: Vec<SignatureParam>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureParam {
    pub name: String,
    pub type_hint: Option<String>,
    pub default_value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextReplacement {
    pub line: usize,
    pub old_text: String,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub file_path: PathBuf,
    pub original_content: String,
    pub modified_content: String,
    pub replacements: Vec<TextReplacement>,
    pub is_included: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefactorPlan {
    pub kind: RefactorKind,
    pub description: String,
    pub file_diffs: Vec<FileDiff>,
    pub can_apply_safely: bool,
    pub warnings: Vec<String>,
}

impl RefactorPlan {
    pub fn apply(&self) -> HashMap<PathBuf, String> {
        let mut results = HashMap::new();
        for diff in &self.file_diffs {
            if diff.is_included {
                results.insert(diff.file_path.clone(), diff.modified_content.clone());
            }
        }
        results
    }
}

pub fn derive_psr4_namespace(base_path: &Path, file_path: &Path, vendor_prefix: &str) -> String {
    let parent = file_path.parent().unwrap_or(Path::new(""));
    let relative = parent.strip_prefix(base_path).unwrap_or(parent);

    let mut parts = Vec::new();
    if !vendor_prefix.is_empty() {
        parts.push(vendor_prefix.trim_end_matches('\\').to_string());
    }

    for component in relative.components() {
        let comp_str = component.as_os_str().to_string_lossy();
        if comp_str == "src" || comp_str == "app" {
            continue;
        }
        parts.push(comp_str.to_string());
    }

    parts.join("\\")
}

pub fn plan_move_class(
    class_name: &str,
    source_path: &Path,
    target_path: &Path,
    old_namespace: &str,
    new_namespace: &str,
    source_content: &str,
    project_files: &[(PathBuf, String)],
) -> RefactorPlan {
    let mut file_diffs = Vec::new();

    let mut moved_replacements = Vec::new();
    let old_ns_decl = format!("namespace {old_namespace};");
    let new_ns_decl = format!("namespace {new_namespace};");

    let mut modified_source = source_content.to_string();
    if let Some(pos) = source_content.find(&old_ns_decl) {
        let line_num = source_content[..pos].lines().count() + 1;
        moved_replacements.push(TextReplacement {
            line: line_num,
            old_text: old_ns_decl.clone(),
            new_text: new_ns_decl.clone(),
        });
        modified_source = modified_source.replacen(&old_ns_decl, &new_ns_decl, 1);
    }

    file_diffs.push(FileDiff {
        file_path: target_path.to_path_buf(),
        original_content: source_content.to_string(),
        modified_content: modified_source,
        replacements: moved_replacements,
        is_included: true,
    });

    let old_import = format!("use {old_namespace}\\{class_name};");
    let new_import = format!("use {new_namespace}\\{class_name};");

    let old_qualified = format!("\\{old_namespace}\\{class_name}");
    let new_qualified = format!("\\{new_namespace}\\{class_name}");

    for (file_path, content) in project_files {
        if file_path == source_path || file_path == target_path {
            continue;
        }

        let mut replacements = Vec::new();
        let mut new_content = content.clone();

        if content.contains(&old_import) {
            for (line_idx, line) in content.lines().enumerate() {
                if line.contains(&old_import) {
                    replacements.push(TextReplacement {
                        line: line_idx + 1,
                        old_text: old_import.clone(),
                        new_text: new_import.clone(),
                    });
                }
            }
            new_content = new_content.replace(&old_import, &new_import);
        }

        if content.contains(&old_qualified) {
            for (line_idx, line) in content.lines().enumerate() {
                if line.contains(&old_qualified) {
                    replacements.push(TextReplacement {
                        line: line_idx + 1,
                        old_text: old_qualified.clone(),
                        new_text: new_qualified.clone(),
                    });
                }
            }
            new_content = new_content.replace(&old_qualified, &new_qualified);
        }

        if !replacements.is_empty() {
            file_diffs.push(FileDiff {
                file_path: file_path.clone(),
                original_content: content.clone(),
                modified_content: new_content,
                replacements,
                is_included: true,
            });
        }
    }

    RefactorPlan {
        kind: RefactorKind::MoveClass {
            class_name: class_name.to_string(),
            source_path: source_path.to_path_buf(),
            target_path: target_path.to_path_buf(),
            old_namespace: old_namespace.to_string(),
            new_namespace: new_namespace.to_string(),
        },
        description: format!("Move class {class_name} from {old_namespace} to {new_namespace}"),
        file_diffs,
        can_apply_safely: true,
        warnings: Vec::new(),
    }
}

pub fn plan_safe_delete(
    symbol_name: &str,
    file_path: &Path,
    source_content: &str,
    project_files: &[(PathBuf, String)],
) -> RefactorPlan {
    let mut warnings = Vec::new();

    for (path, content) in project_files {
        if path == file_path {
            continue;
        }

        for (line_idx, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with('#') {
                continue;
            }

            if line.contains(symbol_name) {
                let file_name = path
                    .file_name()
                    .map(|f| f.to_string_lossy())
                    .unwrap_or_default();
                warnings.push(format!(
                    "Usages found in {}:{} -> {}",
                    file_name,
                    line_idx + 1,
                    trimmed
                ));
            }
        }
    }

    let can_apply_safely = warnings.is_empty();

    let diff = FileDiff {
        file_path: file_path.to_path_buf(),
        original_content: source_content.to_string(),
        modified_content: String::new(),
        replacements: vec![TextReplacement {
            line: 1,
            old_text: source_content.to_string(),
            new_text: String::new(),
        }],
        is_included: can_apply_safely,
    };

    RefactorPlan {
        kind: RefactorKind::SafeDelete {
            symbol_name: symbol_name.to_string(),
            file_path: file_path.to_path_buf(),
        },
        description: format!("Safe delete symbol '{symbol_name}'"),
        file_diffs: vec![diff],
        can_apply_safely,
        warnings,
    }
}

pub fn plan_change_signature(
    function_name: &str,
    def_path: &Path,
    def_content: &str,
    reordered_indices: &[usize],
    added_params: &[SignatureParam],
    caller_files: &[(PathBuf, String)],
) -> RefactorPlan {
    let mut file_diffs = Vec::new();

    let mut def_replacements = Vec::new();
    let mut modified_def = def_content.to_string();

    let func_needle = format!("function {function_name}(");
    if let Some(pos) = def_content.find(&func_needle) {
        let after_paren = &def_content[pos + func_needle.len()..];
        if let Some(close_idx) = after_paren.find(')') {
            let old_params_str = &after_paren[..close_idx];
            let params: Vec<String> = old_params_str
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();

            let mut reordered = Vec::new();
            for &idx in reordered_indices {
                if let Some(p) = params.get(idx) {
                    reordered.push(p.clone());
                }
            }
            for added in added_params {
                let default_part = added
                    .default_value
                    .as_deref()
                    .map(|d| format!(" = {d}"))
                    .unwrap_or_default();
                let type_part = added
                    .type_hint
                    .as_deref()
                    .map(|t| format!("{t} "))
                    .unwrap_or_default();
                reordered.push(format!("{type_part}${}{default_part}", added.name));
            }

            let new_params_str = reordered.join(", ");
            let old_decl = format!("{func_needle}{old_params_str})");
            let new_decl = format!("{func_needle}{new_params_str})");

            let line_num = def_content[..pos].lines().count() + 1;
            def_replacements.push(TextReplacement {
                line: line_num,
                old_text: old_decl.clone(),
                new_text: new_decl.clone(),
            });
            modified_def = modified_def.replacen(&old_decl, &new_decl, 1);
        }
    }

    file_diffs.push(FileDiff {
        file_path: def_path.to_path_buf(),
        original_content: def_content.to_string(),
        modified_content: modified_def,
        replacements: def_replacements,
        is_included: true,
    });

    let call_needle = format!("{function_name}(");
    for (caller_path, content) in caller_files {
        let mut replacements = Vec::new();
        let mut modified_caller = content.clone();

        for (line_idx, line) in content.lines().enumerate() {
            if let Some(pos) = line.find(&call_needle) {
                let after_paren = &line[pos + call_needle.len()..];
                if let Some(close_idx) = after_paren.find(')') {
                    let old_args_str = &after_paren[..close_idx];
                    let args: Vec<String> = old_args_str
                        .split(',')
                        .map(|a| a.trim().to_string())
                        .filter(|a| !a.is_empty())
                        .collect();

                    let mut new_args = Vec::new();
                    for &idx in reordered_indices {
                        if let Some(a) = args.get(idx) {
                            new_args.push(a.clone());
                        }
                    }
                    for added in added_params {
                        let val = added
                            .default_value
                            .as_deref()
                            .unwrap_or("null")
                            .to_string();
                        new_args.push(val);
                    }

                    let new_args_str = new_args.join(", ");
                    let old_call = format!("{call_needle}{old_args_str})");
                    let new_call = format!("{call_needle}{new_args_str})");

                    replacements.push(TextReplacement {
                        line: line_idx + 1,
                        old_text: old_call.clone(),
                        new_text: new_call.clone(),
                    });
                    modified_caller = modified_caller.replace(&old_call, &new_call);
                }
            }
        }

        if !replacements.is_empty() {
            file_diffs.push(FileDiff {
                file_path: caller_path.clone(),
                original_content: content.clone(),
                modified_content: modified_caller,
                replacements,
                is_included: true,
            });
        }
    }

    RefactorPlan {
        kind: RefactorKind::ChangeSignature {
            function_name: function_name.to_string(),
            file_path: def_path.to_path_buf(),
            reordered_indices: reordered_indices.to_vec(),
            added_params: added_params.to_vec(),
        },
        description: format!("Change signature for function '{function_name}'"),
        file_diffs,
        can_apply_safely: true,
        warnings: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_psr4_namespace_derivation() {
        let base = Path::new("/workspace");
        let file = Path::new("/workspace/src/Services/Payments/StripeGateway.php");
        let ns = derive_psr4_namespace(base, file, "App\\");
        assert_eq!(ns, "App\\Services\\Payments");
    }

    #[test]
    fn test_move_class_refactoring() {
        let source_path = PathBuf::from("app/Services/OldService.php");
        let target_path = PathBuf::from("app/Billing/NewService.php");
        let source_content = r#"<?php
namespace App\Services;

class OldService {
    public function handle() {}
}
"#;

        let caller_content = r#"<?php
namespace App\Http\Controllers;

use App\Services\OldService;

class OrderController {
    public function __construct(private OldService $service) {}
}
"#;

        let project_files = vec![(PathBuf::from("app/Http/Controllers/OrderController.php"), caller_content.to_string())];

        let plan = plan_move_class(
            "OldService",
            &source_path,
            &target_path,
            "App\\Services",
            "App\\Billing",
            source_content,
            &project_files,
        );

        assert!(plan.can_apply_safely);
        assert_eq!(plan.file_diffs.len(), 2);

        let applied = plan.apply();
        let moved_file = applied.get(&target_path).expect("moved file should exist");
        assert!(moved_file.contains("namespace App\\Billing;"));

        let updated_caller = applied.get(&PathBuf::from("app/Http/Controllers/OrderController.php")).expect("caller should exist");
        assert!(updated_caller.contains("use App\\Billing\\OldService;"));
    }

    #[test]
    fn test_safe_delete_with_usages_warning() {
        let file_path = PathBuf::from("app/Services/UserService.php");
        let content = "<?php class UserService {}";

        let caller_files = vec![(
            PathBuf::from("app/Controllers/UserController.php"),
            "<?php use App\\Services\\UserService;\nclass UserController { private UserService $s; }".to_string(),
        )];

        let plan = plan_safe_delete("UserService", &file_path, content, &caller_files);
        assert!(!plan.can_apply_safely);
        assert_eq!(plan.warnings.len(), 2);
        assert!(plan.warnings[0].contains("UserController.php"));
    }

    #[test]
    fn test_safe_delete_zero_usages() {
        let file_path = PathBuf::from("app/Services/DeadCode.php");
        let content = "<?php class DeadCode {}";
        let caller_files = vec![(
            PathBuf::from("app/Controllers/OtherController.php"),
            "<?php class OtherController {}".to_string(),
        )];

        let plan = plan_safe_delete("DeadCode", &file_path, content, &caller_files);
        assert!(plan.can_apply_safely);
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn test_change_signature_reordering_and_defaults() {
        let def_path = PathBuf::from("app/functions.php");
        let def_content = r#"<?php
function calculate_tax($subtotal, $rate) {
    return $subtotal * $rate;
}
"#;

        let caller_files = vec![(
            PathBuf::from("app/cart.php"),
            "<?php $tax = calculate_tax($amount, 0.05);".to_string(),
        )];

        let reordered_indices = vec![1, 0];
        let added_params = vec![SignatureParam {
            name: "currency".to_string(),
            type_hint: Some("string".to_string()),
            default_value: Some("'USD'".to_string()),
        }];

        let plan = plan_change_signature(
            "calculate_tax",
            &def_path,
            def_content,
            &reordered_indices,
            &added_params,
            &caller_files,
        );

        assert!(plan.can_apply_safely);
        let applied = plan.apply();

        let modified_def = applied.get(&def_path).expect("def should exist");
        assert!(modified_def.contains("function calculate_tax($rate, $subtotal, string $currency = 'USD')"));

        let modified_caller = applied.get(&PathBuf::from("app/cart.php")).expect("caller should exist");
        assert!(modified_caller.contains("calculate_tax(0.05, $amount, 'USD')"));
    }
}
