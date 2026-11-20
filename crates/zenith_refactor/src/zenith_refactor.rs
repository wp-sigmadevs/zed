use std::collections::HashMap;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

pub mod preview_modal;
pub use preview_modal::*;

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

    pub fn apply_to_disk(&self) -> std::io::Result<Vec<PathBuf>> {
        let mut affected = Vec::new();

        if let RefactorKind::MoveClass {
            source_path,
            target_path,
            ..
        } = &self.kind
        {
            if source_path != target_path && source_path.exists() {
                let _ = std::fs::remove_file(source_path);
            }
        }

        if let RefactorKind::SafeDelete { file_path, .. } = &self.kind {
            if self.file_diffs.is_empty() && file_path.exists() {
                let _ = std::fs::remove_file(file_path);
                affected.push(file_path.clone());
            }
        }

        for diff in &self.file_diffs {
            if diff.is_included {
                if let Some(parent) = diff.file_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&diff.file_path, &diff.modified_content)?;
                affected.push(diff.file_path.clone());
            }
        }

        Ok(affected)
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
    _file_content: &str,
    project_files: &[(PathBuf, String)],
) -> RefactorPlan {
    let mut warnings = Vec::new();
    let mut file_diffs = Vec::new();

    let pattern = format!(r"\b{}\b", regex::escape(symbol_name));
    let reg = regex::Regex::new(&pattern).ok();

    for (p, content) in project_files {
        if p == file_path {
            continue;
        }

        let mut count = 0;
        let mut replacements = Vec::new();

        for (line_idx, line) in content.lines().enumerate() {
            let matches = if let Some(ref r) = reg {
                r.is_match(line)
            } else {
                line.contains(symbol_name)
            };

            if matches {
                count += 1;
                replacements.push(TextReplacement {
                    line: line_idx + 1,
                    old_text: line.trim().to_string(),
                    new_text: String::new(),
                });
            }
        }

        if count > 0 {
            warnings.push(format!(
                "Symbol '{symbol_name}' referenced {count} time(s) in {}",
                p.display()
            ));
            file_diffs.push(FileDiff {
                file_path: p.clone(),
                original_content: content.clone(),
                modified_content: content.clone(),
                replacements,
                is_included: false,
            });
        }
    }

    let can_apply_safely = warnings.is_empty();

    RefactorPlan {
        kind: RefactorKind::SafeDelete {
            symbol_name: symbol_name.to_string(),
            file_path: file_path.to_path_buf(),
        },
        description: format!("Safe delete symbol '{symbol_name}'"),
        file_diffs,
        can_apply_safely,
        warnings,
    }
}

pub fn plan_change_signature(
    function_name: &str,
    def_file: &Path,
    def_content: &str,
    reordered_indices: &[usize],
    added_params: &[SignatureParam],
    caller_files: &[(PathBuf, String)],
) -> RefactorPlan {
    let mut file_diffs = Vec::new();

    let fn_header_pattern = format!(r"function\s+{}\s*\(([^)]*)\)", regex::escape(function_name));
    let Ok(fn_header_reg) = regex::Regex::new(&fn_header_pattern) else {
        return RefactorPlan {
            kind: RefactorKind::ChangeSignature {
                function_name: function_name.to_string(),
                file_path: def_file.to_path_buf(),
                reordered_indices: reordered_indices.to_vec(),
                added_params: added_params.to_vec(),
            },
            description: format!("Change signature for {function_name}"),
            file_diffs: Vec::new(),
            can_apply_safely: false,
            warnings: vec!["Could not parse function signature regex".to_string()],
        };
    };

    let mut def_replacements = Vec::new();
    let mut modified_def = def_content.to_string();

    if let Some(caps) = fn_header_reg.captures(def_content) {
        if let Some(params_match) = caps.get(1) {
            let original_params_str = params_match.as_str();
            let raw_params: Vec<String> = original_params_str
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();

            let mut new_params: Vec<String> = Vec::new();
            for &idx in reordered_indices {
                if let Some(p) = raw_params.get(idx) {
                    new_params.push(p.clone());
                }
            }

            for added in added_params {
                let mut param_str = String::new();
                if let Some(ref th) = added.type_hint {
                    param_str.push_str(th);
                    param_str.push(' ');
                }
                if !added.name.starts_with('$') {
                    param_str.push('$');
                }
                param_str.push_str(&added.name);
                if let Some(ref def_val) = added.default_value {
                    param_str.push_str(" = ");
                    param_str.push_str(def_val);
                }
                new_params.push(param_str);
            }

            let new_params_str = new_params.join(", ");
            let old_full = caps.get(0).unwrap().as_str();
            let new_full = format!("function {function_name}({new_params_str})");

            let line_num = def_content[..caps.get(0).unwrap().start()].lines().count() + 1;
            def_replacements.push(TextReplacement {
                line: line_num,
                old_text: old_full.to_string(),
                new_text: new_full.clone(),
            });

            modified_def = modified_def.replacen(old_full, &new_full, 1);
        }
    }

    file_diffs.push(FileDiff {
        file_path: def_file.to_path_buf(),
        original_content: def_content.to_string(),
        modified_content: modified_def,
        replacements: def_replacements,
        is_included: true,
    });

    let call_pattern = format!(r"\b{}\s*\(([^)]*)\)", regex::escape(function_name));
    if let Ok(call_reg) = regex::Regex::new(&call_pattern) {
        for (caller_path, caller_content) in caller_files {
            if caller_path == def_file {
                continue;
            }

            let mut caller_replacements = Vec::new();
            let mut modified_caller = caller_content.clone();

            for caps in call_reg.captures_iter(caller_content) {
                if let Some(args_match) = caps.get(1) {
                    let old_args_str = args_match.as_str();
                    let raw_args: Vec<String> = old_args_str
                        .split(',')
                        .map(|a| a.trim().to_string())
                        .filter(|a| !a.is_empty())
                        .collect();

                    let mut reordered_args: Vec<String> = Vec::new();
                    for &idx in reordered_indices {
                        if let Some(arg) = raw_args.get(idx) {
                            reordered_args.push(arg.clone());
                        }
                    }

                    for added in added_params {
                        if let Some(ref def_val) = added.default_value {
                            reordered_args.push(def_val.clone());
                        } else {
                            reordered_args.push("null".to_string());
                        }
                    }

                    let new_args_str = reordered_args.join(", ");
                    let old_call = caps.get(0).unwrap().as_str();
                    let new_call = format!("{function_name}({new_args_str})");

                    let line_num = caller_content[..caps.get(0).unwrap().start()].lines().count() + 1;
                    caller_replacements.push(TextReplacement {
                        line: line_num,
                        old_text: old_call.to_string(),
                        new_text: new_call.clone(),
                    });

                    modified_caller = modified_caller.replacen(old_call, &new_call, 1);
                }
            }

            if !caller_replacements.is_empty() {
                file_diffs.push(FileDiff {
                    file_path: caller_path.clone(),
                    original_content: caller_content.clone(),
                    modified_content: modified_caller,
                    replacements: caller_replacements,
                    is_included: true,
                });
            }
        }
    }

    RefactorPlan {
        kind: RefactorKind::ChangeSignature {
            function_name: function_name.to_string(),
            file_path: def_file.to_path_buf(),
            reordered_indices: reordered_indices.to_vec(),
            added_params: added_params.to_vec(),
        },
        description: format!("Change signature for {function_name}"),
        file_diffs,
        can_apply_safely: true,
        warnings: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    #[test]
    fn test_derive_psr4_namespace() {
        let base = Path::new("/var/www/my-project");
        let file = Path::new("/var/www/my-project/app/Services/Billing/PaymentService.php");
        let ns = derive_psr4_namespace(base, file, "App");
        assert_eq!(ns, "App\\Services\\Billing");
    }

    #[test]
    fn test_move_class_plan() {
        let source_path = PathBuf::from("app/Services/OldService.php");
        let target_path = PathBuf::from("app/Billing/OldService.php");
        let source_content = r#"<?php
namespace App\Services;

class OldService {
    public function execute() {}
}
"#;

        let caller_files = vec![(
            PathBuf::from("app/Http/Controllers/OrderController.php"),
            r#"<?php
namespace App\Http\Controllers;

use App\Services\OldService;

class OrderController {
    public function handle(OldService $service) {
        $service->execute();
    }
}
"#
            .to_string(),
        )];

        let plan = plan_move_class(
            "OldService",
            &source_path,
            &target_path,
            "App\\Services",
            "App\\Billing",
            source_content,
            &caller_files,
        );

        assert!(plan.can_apply_safely);
        assert_eq!(plan.file_diffs.len(), 2);

        let applied = plan.apply();
        let moved_file = applied.get(&target_path).expect("moved file should exist");
        assert!(moved_file.contains("namespace App\\Billing;"));

        let updated_caller = applied
            .get(&PathBuf::from("app/Http/Controllers/OrderController.php"))
            .expect("caller should exist");
        assert!(updated_caller.contains("use App\\Billing\\OldService;"));
    }

    #[test]
    fn test_safe_delete_with_usages_warning() {
        let file_path = PathBuf::from("app/Services/UserService.php");
        let content = "<?php class UserService {}";

        let caller_files = vec![(
            PathBuf::from("app/Controllers/UserController.php"),
            "<?php use App\\Services\\UserService;\nclass UserController { private UserService $s; }"
                .to_string(),
        )];

        let plan = plan_safe_delete("UserService", &file_path, content, &caller_files);
        assert!(!plan.can_apply_safely);
        assert_eq!(plan.warnings.len(), 1);
        assert_eq!(plan.file_diffs[0].replacements.len(), 2);
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

        let modified_caller = applied
            .get(&PathBuf::from("app/cart.php"))
            .expect("caller should exist");
        assert!(modified_caller.contains("calculate_tax(0.05, $amount, 'USD')"));
    }

    #[test]
    fn test_apply_to_disk_and_exclusions() {
        let temp_dir = tempfile::tempdir().unwrap();
        let base = temp_dir.path();

        let src_file = base.join("app/Services/OldService.php");
        let target_file = base.join("app/Billing/OldService.php");
        let caller_file = base.join("app/Controllers/OrderController.php");

        std::fs::create_dir_all(src_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(caller_file.parent().unwrap()).unwrap();

        let src_content = "<?php\nnamespace App\\Services;\nclass OldService {}";
        let caller_content = "<?php\nuse App\\Services\\OldService;\nclass OrderController {}";

        std::fs::write(&src_file, src_content).unwrap();
        std::fs::write(&caller_file, caller_content).unwrap();

        let project_files = vec![(caller_file.clone(), caller_content.to_string())];

        let mut plan = plan_move_class(
            "OldService",
            &src_file,
            &target_file,
            "App\\Services",
            "App\\Billing",
            src_content,
            &project_files,
        );

        assert_eq!(plan.file_diffs.len(), 2);

        // Exclude the caller file from application
        plan.file_diffs[1].is_included = false;

        let affected = plan.apply_to_disk().unwrap();
        assert_eq!(affected.len(), 1);
        assert_eq!(affected[0], target_file);

        let written_target = std::fs::read_to_string(&target_file).unwrap();
        assert!(written_target.contains("namespace App\\Billing;"));

        assert!(!src_file.exists());

        let untouched_caller = std::fs::read_to_string(&caller_file).unwrap();
        assert!(untouched_caller.contains("use App\\Services\\OldService;"));
    }

    #[gpui::test]
    fn test_preview_modal_inclusion_toggle(cx: &mut gpui::App) {
        let source_path = PathBuf::from("app/Services/OldService.php");
        let target_path = PathBuf::from("app/Billing/OldService.php");
        let source_content = "<?php\nnamespace App\\Services;\nclass OldService {}";
        let caller_files = vec![(
            PathBuf::from("app/Controllers/OrderController.php"),
            "<?php\nuse App\\Services\\OldService;\nclass OrderController {}".to_string(),
        )];

        let plan = plan_move_class(
            "OldService",
            &source_path,
            &target_path,
            "App\\Services",
            "App\\Billing",
            source_content,
            &caller_files,
        );

        let modal = cx.new(|cx| {
            RefactorPreviewModal {
                plan,
                selected_file_index: 0,
                focus_handle: cx.focus_handle(),
                workspace: gpui::WeakEntity::new_invalid(),
                is_applying: false,
                status_message: None,
            }
        });

        modal.update(cx, |this, cx| {
            assert_eq!(this.plan.file_diffs.len(), 2);
            assert!(this.plan.file_diffs[0].is_included);
            assert!(this.plan.file_diffs[1].is_included);

            // Toggle inclusion of first file
            this.toggle_file_inclusion(0, cx);
            assert!(!this.plan.file_diffs[0].is_included);

            // Select None
            this.set_all_files_included(false, cx);
            assert!(!this.plan.file_diffs[0].is_included);
            assert!(!this.plan.file_diffs[1].is_included);

            // Select All
            this.set_all_files_included(true, cx);
            assert!(this.plan.file_diffs[0].is_included);
            assert!(this.plan.file_diffs[1].is_included);

            // Select file
            this.select_file(1, cx);
            assert_eq!(this.selected_file_index, 1);
        });
    }
}
