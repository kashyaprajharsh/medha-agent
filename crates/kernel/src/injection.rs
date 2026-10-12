//! Content attacks, separate from the policy for executing commands.

use regex::Regex;
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Risk {
    pub id: &'static str,
    pub dangerous: bool,
}

fn patterns() -> &'static [(Risk, Regex)] {
    static PATTERNS: OnceLock<Vec<(Risk, Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            ("instruction_override", false, r"(?i)\b(ignore|disregard|forget|discard|override)\b[^.\n]{0,60}\b(previous|prior|earlier|above|original|system)\b[^.\n]{0,60}\b(instructions?|prompts?|rules?|guidelines?|directives?|messages?)\b"),
            ("prompt_disclosure", false, r"(?i)\b(reveal|show|print|repeat|output|echo|leak|expose|dump)\b[^.\n]{0,60}\b(system[ -]?(prompt|message|instructions?)|hidden (prompt|instructions?))\b"),
            ("forged_role", true, r"(?i)(<\|(im_start|im_end|system|user|assistant|endoftext)\|>|\[/?INST\]|<</?SYS>>|\bBEGIN SYSTEM PROMPT\b|\[system\]\(#.*\))"),
            ("safety_bypass", false, r"(?i)\b(disable|turn off|bypass|circumvent|evade|ignore|skip|override|remove)\b[^.\n]{0,40}\b(safety|guardrails?|security|sandbox|polic(y|ies)|restrictions?|protection|filters?)\b"),
            ("secret_exfiltration", false, r"(?i)\b(send|upload|post|exfiltrate|transmit|forward|leak|curl|wget)\b[^.\n]{0,80}([a-z0-9_]*(secret|api[ _-]?key|access[ _-]?key|token|private[ _-]?key|password|credential)[a-z0-9_]*|\.ssh|\.env\b|environment variable)"),
            ("secret_exfiltration", false, r"(?i)([a-z0-9_]*(secret|api[ _-]?key|token|private[ _-]?key|password|credential)[a-z0-9_]*|\.ssh|\.env\b)[^.\n]{0,80}\b(send|upload|post|exfiltrate|transmit|forward|leak)\b"),
            ("approval_bypass", false, r"(?i)\b(do not|don't|never|no need to|without)\b[^.\n]{0,30}\b(ask|asking|confirm|confirming|notify|telling|inform)\b[^.\n]{0,30}\b(user|human|permission|approval|confirmation)\b"),
            ("role_hijack", false, r"(?i)\b(you are now|developer mode|jailbreak|unrestricted mode)\b[^.\n]{0,40}\b(unrestricted|no restrictions|free|enabled|activated|assistant|agent)\b"),
        ]
        .into_iter()
        .map(|(id, dangerous, pattern)| (Risk { id, dangerous }, Regex::new(pattern).unwrap()))
        .collect()
    })
}

pub fn scan(text: &str) -> Vec<Risk> {
    // Normalize the whole admitted input; a suffix must not escape scanning.
    let normalized: String = text.nfkc().filter(|ch| !invisible(*ch)).collect();
    let mut risks = Vec::new();
    static NEGATION: OnceLock<Regex> = OnceLock::new();
    let negation =
        NEGATION.get_or_init(|| Regex::new(r"(?i)\b(never|do not|don't|must not)\s*$").unwrap());
    for (risk, pattern) in patterns() {
        let positive = pattern.find_iter(&normalized).any(|found| {
            risk.dangerous
                || !negation.is_match(
                    normalized[..found.start()]
                        .rsplit('\n')
                        .next()
                        .unwrap_or_default(),
                )
        });
        if positive && !risks.contains(risk) {
            risks.push(*risk);
        }
    }
    if text.chars().any(|ch| matches!(ch, '\u{e0000}'..='\u{e007f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')) {
        risks.push(Risk { id: "hidden_unicode", dangerous: true });
    }
    risks
}

fn invisible(ch: char) -> bool {
    matches!(
        ch,
        '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}' | '\u{00ad}'
    )
}

#[cfg(test)]
#[path = "injection_tests.rs"]
mod tests;
