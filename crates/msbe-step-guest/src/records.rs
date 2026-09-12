//! The records a step exchanges with the host.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// What the host asks a step to do: run over one mod for one loader.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// The plan step's identifier.
    pub step: String,
    /// The mod's name in the profile.
    pub module: String,
    /// The selected loader.
    pub loader: String,
    /// The instance's game version, when it has one.
    #[serde(default)]
    pub game_version: Option<String>,
    /// The step's parameters, with `{game_version}` filled in.
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    /// Instance-relative directories that operations may write beneath.
    #[serde(default)]
    pub roots: Vec<String>,
}

/// An effect a step asks the host to have. The host checks it against the step's declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Operation {
    /// Places the mod's file at `source` at the instance-relative `path`.
    Place {
        /// The file's path inside the mod.
        source: String,
        /// The instance-relative destination.
        path: String,
    },
    /// Writes generated text to the instance-relative `path`.
    WriteFile {
        /// The instance-relative destination.
        path: String,
        /// The file's contents.
        text: String,
    },
}

/// An installer question. The host answers it from the mod's recorded answers, or its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Question {
    /// Stable identifier the answer is recorded under: ASCII letters, digits, `-` and `_`.
    pub id: String,
    /// What the user is asked.
    pub prompt: String,
    /// The kind of answer expected.
    pub kind: QuestionKind,
    /// The choices, for [`QuestionKind::Choice`] and [`QuestionKind::Multi`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<Choice>,
    /// The answer used when none is recorded. Without one, an unanswered question stops the run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

impl Question {
    /// A question answered by exactly one of `choices`, recorded as its identifier.
    pub fn choice(id: impl Into<String>, prompt: impl Into<String>, choices: Vec<Choice>) -> Self {
        Self::new(id, prompt, QuestionKind::Choice, choices)
    }

    /// A question answered by any number of `choices`, recorded as comma-separated identifiers.
    pub fn multi(id: impl Into<String>, prompt: impl Into<String>, choices: Vec<Choice>) -> Self {
        Self::new(id, prompt, QuestionKind::Multi, choices)
    }

    /// A yes-or-no question, recorded as `true` or `false`.
    pub fn boolean(id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self::new(id, prompt, QuestionKind::Boolean, Vec::new())
    }

    /// A free-text question.
    pub fn text(id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self::new(id, prompt, QuestionKind::Text, Vec::new())
    }

    /// This question with `default` as the answer used when none is recorded.
    #[must_use]
    pub fn with_default(mut self, default: impl Into<String>) -> Self {
        self.default = Some(default.into());
        self
    }

    fn new(
        id: impl Into<String>,
        prompt: impl Into<String>,
        kind: QuestionKind,
        choices: Vec<Choice>,
    ) -> Self {
        Self {
            id: id.into(),
            prompt: prompt.into(),
            kind,
            choices,
            default: None,
        }
    }
}

/// The kind of answer a [`Question`] expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuestionKind {
    /// Exactly one choice.
    Choice,
    /// Any number of choices.
    Multi,
    /// `true` or `false`.
    Boolean,
    /// Free text.
    Text,
}

/// One answer a choice question offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Choice {
    /// Stable identifier recorded as the answer: ASCII letters, digits, `-` and `_`.
    pub id: String,
    /// What the user sees.
    pub label: String,
}

impl Choice {
    /// A choice recorded as `id` and shown as `label`.
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}
