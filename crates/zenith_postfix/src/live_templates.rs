//! Built-in live templates: an abbreviation, a description and a body that
//! `template_to_snippet` turns into an editor snippet. `$NAME$` / `$NAME=default$`
//! (upper-case names) are tab stops, repeats mirror, built-ins like `$CLASS_NAME$` are
//! filled in, `$END$` is the last stop and `$$` is a literal `$`.

use crate::LanguageKind;

pub struct LiveTemplate {
    pub abbreviation: &'static str,
    pub description: &'static str,
    pub body: &'static str,
}

const PHP: &[LiveTemplate] = &[
    LiveTemplate {
        abbreviation: "pubf",
        description: "public function",
        body: "public function $NAME$($PARAMETERS$)\n{\n    $END$\n}",
    },
    LiveTemplate {
        abbreviation: "prif",
        description: "private function",
        body: "private function $NAME$($PARAMETERS$)\n{\n    $END$\n}",
    },
    LiveTemplate {
        abbreviation: "ctor",
        description: "constructor",
        body: "public function __construct($PARAMETERS$)\n{\n    $END$\n}",
    },
    LiveTemplate {
        abbreviation: "fore",
        description: "foreach loop",
        body: "foreach ($$$ITERABLE=items$ as $$$ITEM=item$) {\n    $END$\n}",
    },
    LiveTemplate {
        abbreviation: "cls",
        description: "class named after the file",
        body: "class $CLASS_NAME$\n{\n    $END$\n}",
    },
];

const JS: &[LiveTemplate] = &[
    LiveTemplate {
        abbreviation: "clg",
        description: "console.log",
        body: "console.log($END$);",
    },
    LiveTemplate {
        abbreviation: "fn",
        description: "function",
        body: "function $NAME$($PARAMETERS$) {\n    $END$\n}",
    },
    LiveTemplate {
        abbreviation: "afn",
        description: "arrow function",
        body: "const $NAME$ = ($PARAMETERS$) => {\n    $END$\n};",
    },
    LiveTemplate {
        abbreviation: "fore",
        description: "for...of loop",
        body: "for (const $ITEM=item$ of $ITERABLE=items$) {\n    $END$\n}",
    },
    LiveTemplate {
        abbreviation: "cls",
        description: "class named after the file",
        body: "class $CLASS_NAME$ {\n    $END$\n}",
    },
];

pub fn live_templates(lang: LanguageKind) -> &'static [LiveTemplate] {
    if lang.is_php() { PHP } else { JS }
}
