use crate::ast::{Heredoc, Instruction, ParsedAgentfile, Span, Value};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("line {line}: empty or invalid instruction")]
    InvalidInstruction { line: usize },
    #[error("line {line}: instruction keyword must be uppercase")]
    KeywordMustBeUppercase { line: usize },
    #[error("line {line}: unterminated quoted string")]
    UnterminatedString { line: usize },
    #[error("line {line}: expected heredoc terminator tag after <<")]
    MissingHeredocTag { line: usize },
    #[error("line {line}: unterminated heredoc, expected closing tag `{tag}`")]
    UnterminatedHeredoc { line: usize, tag: String },
}

pub fn parse_agentfile(input: &str) -> Result<ParsedAgentfile, ParseError> {
    let normalized = input.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.lines().collect();
    let mut instructions = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line_no = index + 1;
        let raw = lines[index];
        let trimmed = raw.trim();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }

        let tokens = tokenize(trimmed, line_no)?;
        if tokens.is_empty() {
            return Err(ParseError::InvalidInstruction { line: line_no });
        }

        let keyword = match &tokens[0] {
            Token::Word(value) | Token::Quoted(value) => value.clone(),
            Token::HeredocStart(_) => {
                return Err(ParseError::InvalidInstruction { line: line_no });
            }
        };

        if keyword.chars().any(|ch| ch.is_ascii_lowercase()) {
            return Err(ParseError::KeywordMustBeUppercase { line: line_no });
        }

        let mut args = Vec::new();
        let mut line_end = line_no;
        let mut token_index = 1usize;

        while token_index < tokens.len() {
            match &tokens[token_index] {
                Token::Word(value) => args.push(Value::Token(value.clone())),
                Token::Quoted(value) => args.push(Value::String(value.clone())),
                Token::HeredocStart(tag) => {
                    let (body, next_index) = parse_heredoc(&lines, index + 1, tag, line_no)?;
                    args.push(Value::Heredoc(Heredoc {
                        tag: tag.clone(),
                        body,
                    }));
                    index = next_index;
                    line_end = index + 1;
                }
            }
            token_index += 1;
        }

        instructions.push(Instruction {
            keyword,
            args,
            span: Span {
                line_start: line_no,
                line_end,
            },
        });

        index += 1;
    }

    Ok(ParsedAgentfile { instructions })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Word(String),
    Quoted(String),
    HeredocStart(String),
}

fn tokenize(line: &str, line_no: usize) -> Result<Vec<Token>, ParseError> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0usize;

    while index < chars.len() {
        while index < chars.len() && chars[index].is_whitespace() {
            index += 1;
        }
        if index >= chars.len() {
            break;
        }

        match chars[index] {
            '"' => {
                index += 1;
                let start = index;
                while index < chars.len() && chars[index] != '"' {
                    index += 1;
                }
                if index >= chars.len() {
                    return Err(ParseError::UnterminatedString { line: line_no });
                }
                let value: String = chars[start..index].iter().collect();
                tokens.push(Token::Quoted(value));
                index += 1;
            }
            '<' if index + 1 < chars.len() && chars[index + 1] == '<' => {
                index += 2;
                let start = index;
                while index < chars.len() && !chars[index].is_whitespace() {
                    index += 1;
                }
                if start == index {
                    return Err(ParseError::MissingHeredocTag { line: line_no });
                }
                let tag: String = chars[start..index].iter().collect();
                tokens.push(Token::HeredocStart(tag));
            }
            _ => {
                let start = index;
                while index < chars.len() && !chars[index].is_whitespace() {
                    index += 1;
                }
                let value: String = chars[start..index].iter().collect();
                tokens.push(Token::Word(value));
            }
        }
    }

    Ok(tokens)
}

fn parse_heredoc(
    lines: &[&str],
    start_index: usize,
    tag: &str,
    instruction_line: usize,
) -> Result<(String, usize), ParseError> {
    let mut body = Vec::new();
    let mut index = start_index;

    while index < lines.len() {
        if lines[index].trim() == tag {
            return Ok((body.join("\n"), index));
        }
        body.push(lines[index].to_string());
        index += 1;
    }

    Err(ParseError::UnterminatedHeredoc {
        line: instruction_line,
        tag: tag.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_file() {
        let parsed = parse_agentfile("FROM example/remote-worker:latest\nNAME demo\n").unwrap();
        assert_eq!(parsed.instructions.len(), 2);
        assert_eq!(parsed.instructions[0].keyword, "FROM");
        assert_eq!(
            parsed.instructions[0].args,
            vec![Value::Token("example/remote-worker:latest".to_string())]
        );
    }

    #[test]
    fn parses_quoted_strings() {
        let parsed = parse_agentfile("LABEL foo \"bar baz\"\n").unwrap();
        assert_eq!(
            parsed.instructions[0].args,
            vec![
                Value::Token("foo".to_string()),
                Value::String("bar baz".to_string())
            ]
        );
    }

    #[test]
    fn parses_heredoc() {
        let parsed = parse_agentfile("PROMPT <<EOF\nhello\nworld\nEOF\n").unwrap();
        assert_eq!(parsed.instructions.len(), 1);
        assert_eq!(parsed.instructions[0].span.line_end, 4);
        match &parsed.instructions[0].args[0] {
            Value::Heredoc(doc) => assert_eq!(doc.body, "hello\nworld"),
            other => panic!("expected heredoc, got {other:?}"),
        }
    }

    #[test]
    fn rejects_lowercase_keywords() {
        let err = parse_agentfile("from foo\n").unwrap_err();
        assert_eq!(err, ParseError::KeywordMustBeUppercase { line: 1 });
    }
}
