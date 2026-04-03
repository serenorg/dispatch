use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ParsedAgentfile {
    pub instructions: Vec<Instruction>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Instruction {
    pub keyword: String,
    pub args: Vec<Value>,
    pub span: Span,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    Token(String),
    String(String),
    Heredoc(Heredoc),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Heredoc {
    pub tag: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Span {
    pub line_start: usize,
    pub line_end: usize,
}
