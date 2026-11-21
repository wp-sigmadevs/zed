//! Built-in live templates: an abbreviation, a description and a body that
//! `expand_live_template` fills in. `$END$` marks where the cursor lands.

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
        body: "public function $END$()\n{\n    \n}",
    },
    LiveTemplate {
        abbreviation: "prif",
        description: "private function",
        body: "private function $END$()\n{\n    \n}",
    },
    LiveTemplate {
        abbreviation: "ctor",
        description: "constructor",
        body: "public function __construct($END$)\n{\n    \n}",
    },
    LiveTemplate {
        abbreviation: "fore",
        description: "foreach loop",
        body: "foreach ($END$ as $item) {\n    \n}",
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
        body: "function $END$() {\n    \n}",
    },
    LiveTemplate {
        abbreviation: "afn",
        description: "arrow function",
        body: "const $END$ = () => {\n    \n};",
    },
    LiveTemplate {
        abbreviation: "fore",
        description: "for...of loop",
        body: "for (const item of $END$) {\n    \n}",
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
