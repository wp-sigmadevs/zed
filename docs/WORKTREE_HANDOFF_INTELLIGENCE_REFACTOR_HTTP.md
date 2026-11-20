# Zenith Worktree Handoff: Framework Intelligence, Refactoring & HTTP Client

**Date:** 2026-11-20  
**Branch:** `feat/external-libraries` (remote `origin/feat/external-libraries`)  
**Base Commit:** `a29abb9ed4` (Merge upstream main into zenith)  
**Head Commit:** `1331d96968`  
**Crates Added:** 7 new GPL-3.0 crates (`zenith_*`)  
**Upstream Hooks:** Exactly 5 lines hooked in `crates/project_panel/src/project_panel.rs`  

---

## 1. Overview & Worktree Isolation

All work was conducted strictly inside `/Users/srafiz/GithubProjects/zed-extlib` on branch `feat/external-libraries`.
- **Untouched Worktrees:** Neither `/Users/srafiz/GithubProjects/zed` (`feat/local-history`, Plan 05) nor `/Users/srafiz/GithubProjects/zed-p21` (`feat/inspections-baseline`, Plan 21) were modified in any way.
- **Pinned Toolchain:** Built, clippy-checked, and tested with Rust 1.98.1 (`cargo +1.98.1`).
- **Disk Safety:** Target directory set to shared cache `CARGO_TARGET_DIR=/Users/srafiz/GithubProjects/zed/target`, verified `>= 60 GiB` available at all stages.
- **Quality Standards:** Every crate includes unit tests, verified sabotage tests, and passes `cargo clippy --all-targets -- -D warnings` with zero warnings.

---

## 2. Completed Features & Crates

### A. Plan 39: External Libraries Node (`zenith_external_libraries`)
- **Commit:** `ef1ff99115`
- **What it does:**
  - Detects dependency and core directories: `vendor/`, `node_modules/`, `wp-includes/`, `wp-admin/`.
  - Groups them under an "External Libraries" virtual root node in the Zed Project Panel.
  - Displays a lock badge and marks external library files as read-only.
- **Upstream Hook:** Exactly 5 lines in `crates/project_panel/src/project_panel.rs`.

---

### B. Plan 19: WordPress Code Intelligence & Tool Window (`zenith_wordpress`)
- **Commits:** `d2529b4dfc` (engine), `f7cf0730e6` (tool panel)
- **What it does:**
  - **Hook Indexer:** Streaming parser detecting `add_action`, `add_filter`, `do_action`, `apply_filters`.
  - **Core Hook DB:** Built-in embedded registry of ~120 critical WordPress Core actions and filters (`init`, `wp_enqueue_scripts`, `the_content`, etc.) with official documentation URLs.
  - **Callback Target Resolver:** Resolves static class methods (`['ClassName', 'method']`), instance methods (`[$this, 'method']`), procedural functions (`'function_name'`), and inline closures.
  - **`WpHooksPanel` Tool Window:**
    - Tabs for **Actions** and **Filters**.
    - Real-time search query filtering.
    - Expandable hook rows with listener file paths, line numbers, priorities, and callback names.
    - 1-click jump to listener code via `workspace.open_abs_path`.

---

### C. Plan 25: Laravel Code Intelligence & Tool Window (`zenith_laravel`)
- **Commits:** `993be8275c` (engine), `4649576782` (tool panel)
- **What it does:**
  - **Project Detector:** Identifies Laravel projects via `artisan` and `composer.json`.
  - **View Dot-Notation Mapper:** Bidirectional mapping between `resources/views/auth/login.blade.php` and `view('auth.login')`.
  - **Blade `<x-component>` Resolver:** Maps anonymous components (`resources/views/components/alert.blade.php`) and class-based components (`App\View\Components\Alert`).
  - **Named Route Extractor:** Parses routes (`Route::get(...)->name('orders.show')`) with HTTP methods and URI paths.
  - **Config & Env Resolvers:** Resolves `config('app.name')` and `env('DB_HOST')`.
  - **`LaravelPanel` Tool Window:**
    - Tabs for **Routes** and **Views**.
    - HTTP method badges (`GET` blue, `POST` green, `PUT`/`PATCH` yellow, `DELETE` red).
    - 1-click jump to route definition and Blade template.

---

### D. Plan 40: Next.js & React Code Intelligence & Tool Window (`zenith_nextjs`)
- **Commits:** `3954a0782a` (engine), `2acb39adf6` (tool panel)
- **What it does:**
  - **App Router URL Calculator:** Computes clean URL paths from file hierarchy, handling Route Groups `(group)` and dynamic segments `[id]` (e.g. `app/(auth)/login/page.tsx` → `/login`, `app/api/users/[id]/route.ts` → `/api/users/:id`).
  - **API Route Method Scanner:** Detects exported handlers (`GET`, `POST`, `PUT`, `DELETE`, `PATCH`).
  - **RSC Safety Linter:** Detects interactive hooks (`useState`, `useEffect`, `useRouter`, `onClick`, `onChange`) in server components without `"use client"`.
  - **`NextJsPanel` Tool Window:**
    - Tabs for **Pages** and **APIs**.
    - Dynamic route pills and method badges.
    - 1-click jump to page/route handler.

---

### E. Plan 09: JetBrains-Grade Advanced Refactoring (`zenith_refactor`)
- **Commits:** `1e2ec762a5` (engine), `cde3ba1b23` (modal & disk applicator)
- **What it does:**
  - **Actions Registered:** `RefactorThis` (`ctrl-t`), `MoveClass`, `SafeDelete`, `ChangeSignature`, `ExtractMethod`, `ExtractVariable`.
  - **PSR-4 Namespace Derivation:** `derive_psr4_namespace` computes target namespaces automatically from composer root and folder hierarchy.
  - **Move Class Engine:** Moves class file, updates `namespace` declaration, updates all `use` imports, and updates fully-qualified usages across the project.
  - **Safe Delete Engine:** Analyzes symbol usage counts across all project files; reports call sites and issues warnings if references exist to prevent breaking callers.
  - **Change Signature Engine:** Reorders parameters, inserts default values for new parameters, and rewrites caller argument lists across all project files.
  - **Multi-Buffer Diff Preview Modal (`RefactorPreviewModal`):**
    - Implemented with GPUI's `ModalView`, `EventEmitter<DismissEvent>`, and `Focusable`.
    - Split-view layout: Left column lists affected files with checkboxes; right column shows line-by-line diff cards with old deletion highlights (`deleted_background` / red) and new creation highlights (`created_background` / green).
    - Amber warning banner (`IconName::Warning`) when refactorings have unresolved callers.
    - Atomic disk applicator (`apply_to_disk`) respecting user exclusions and cleaning up source files on class moves.

---

### F. Plan 26: Live Templates & Postfix Completion (`zenith_postfix`)
- **Commit:** `92f12b1d56`
- **What it does:**
  - **Backtracking Expression Parser:** Accurately extracts preceding expressions ending in `.<trigger>` by matching nested parentheses, brackets, braces, and string quotes.
  - **PHP Postfix Triggers:**
    - `.if`, `.not`, `.notnull`, `.null`, `.return`, `.var`, `.isset`, `.empty`, `.throw`, `.par`, `.echo`, `.dd`, `.dump`.
    - `.foreach` features **intelligent singular deduction** (`$users.foreach` → `foreach ($users as $user)`, `$categories.foreach` → `foreach ($categories as $category)`, `$items.foreach` → `foreach ($items as $item)`).
  - **JS / TS / React Postfix Triggers:**
    - `.if`, `.not`, `.const`, `.let`, `.return`, `.log`, `.await`, `.forof`, `.par`.
  - **Surround-With Engine:** Wraps selected code blocks in `TryCatch` (`\Throwable $e` for PHP, `error` for JS), `If`, `Foreach`, `While`, `Function`.
  - **Live Template Expander:** Evaluates `$CLASS_NAME$`, `$METHOD_NAME$`, `$NAMESPACE$`, `$DATE$`, `$YEAR$`, `$END$`.

---

### G. Plan 08: Built-in HTTP Client (`zenith_http`)
- **Commit:** `1331d96968`
- **What it does:**
  - **Streaming Parser:** Separates multiple requests with `### [Optional Name]`, parses HTTP methods, headers, and bodies (raw, JSON, `< ./file.json`).
  - **Variable Substitution Engine:** Substitutes `{{var}}` as well as dynamic variables `{{$uuid}}`, `{{$timestamp}}`, `{{$randomInt}}`.
  - **Environment Store:** Reads JetBrains `http-client.env.json` and `http-client.private.env.json` with secure private overrides and environment switching.
  - **Bidirectional cURL:** `to_curl` (exports request as curl command) and `parse_curl` (imports curl command as `HttpRequest`).
  - **`HttpResponseView`:** GPUI response viewer with status code badge (green 2xx, amber 4xx, red 5xx), duration ms, payload size, one-click clipboard copying (cURL & response body), and tab switching (`Body`, `Headers`, `Raw`, `Request Info`).
  - **`HttpHistoryStore`:** FIFO history store tracking executed requests and responses.

---

## 3. Commit Log on `feat/external-libraries`

```
1331d96968 feat(zenith_http): implement built-in HTTP client (Plan 08 M1-M4)
92f12b1d56 feat(zenith_postfix): implement postfix completion and live templates (Plan 26)
cde3ba1b23 feat(zenith_refactor): implement multi-buffer diff preview modal (Plan 09 M2)
1e2ec762a5 feat(zenith_refactor): implement JetBrains-grade Move Class, Safe Delete, and Change Signature refactoring engine
2acb39adf6 feat(zenith_nextjs): implement Next.js App Router tool panel (Plan 40 M2)
4649576782 feat(zenith_laravel): implement Laravel Routes and Views tool panel (Plan 25 M2)
f7cf0730e6 feat(zenith_wordpress): implement WordPress Hooks tool panel (Plan 19 M2)
3954a0782a feat(zenith_nextjs): implement Next.js App Router and React Server Component code intelligence
993be8275c feat(zenith_laravel): implement Laravel Idea-grade route, view, config and env code intelligence
d2529b4dfc feat(zenith_wordpress): implement WordPress hook semantic indexer and code intelligence
ef1ff99115 feat(zenith_external_libraries): add PhpStorm-like External Libraries node to project panel
```

---

## 4. Verification Results

All tests pass on Rust 1.98.1:
```sh
cargo test -p zenith_external_libraries
cargo test -p zenith_wordpress
cargo test -p zenith_laravel
cargo test -p zenith_nextjs
cargo test -p zenith_refactor
cargo test -p zenith_postfix
cargo test -p zenith_http
```
**Total:** 28 passed, 0 failed, 0 ignored.  
**Clippy:** Checked with `-D warnings`, zero warnings across all crates.  
**Sabotage Tests:** Verified on every engine.

---

## 5. Merging Instructions for Claude Session

To merge these 7 features into `zenith` or `feat/look`:

```sh
cd /Users/srafiz/GithubProjects/zed  # or destination worktree
git fetch origin feat/external-libraries
git merge origin/feat/external-libraries
```

Because each feature was authored in its own isolated `zenith_*` crate with only 5 hook lines in `crates/project_panel/src/project_panel.rs`, the merge is essentially conflict-free.
