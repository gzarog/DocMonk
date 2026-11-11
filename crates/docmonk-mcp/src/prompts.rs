//! MCP prompts (ADR 0045): DocMonk's two evidence-first workflows as
//! prompts a user picks in their client. A prompt is fixed text around
//! the user's argument; nothing is read from the index to build it.

use serde_json::{json, Value};

/// The longest argument a prompt accepts, in characters.
pub const MAX_ARGUMENT_CHARS: usize = 4000;

struct Prompt {
    name: &'static str,
    title: &'static str,
    description: &'static str,
    argument: &'static str,
    argument_description: &'static str,
    /// The message, with `{}` where the argument goes.
    template: &'static str,
}

const PROMPTS: &[Prompt] = &[
    Prompt {
        name: "answer_with_evidence",
        title: "Answer with evidence",
        description: "Answer a question from the indexed code and documents, citing every \
            claim or saying the evidence is insufficient.",
        argument: "question",
        argument_description: "The question to answer.",
        template: "Answer this question from the DocMonk index: {}\n\n\
            1. Call docmonk_explore with the question to see which code and documents are \
            relevant.\n\
            2. Call docmonk_evidence with the question (add source, path or document filters \
            if step 1 showed where the answer lives) to get verified [E#] citations and a \
            verdict.\n\
            3. If the verdict is insufficient_evidence, say that the index does not answer \
            the question and what is missing. Do not answer from general knowledge.\n\
            4. Otherwise answer using only the evidence, and put the [E#] citation after \
            every claim. If the verdict is partial, say which part is not covered.\n\
            5. To quote or check a citation, pass its evidence entry to docmonk_read.",
    },
    Prompt {
        name: "impact_review",
        title: "Impact review",
        description: "Review what a change to a code symbol affects: callers, callees, \
            tests and the specifications linked to it.",
        argument: "symbol",
        argument_description: "The symbol to review: a name or a fully qualified name.",
        template: "Review the impact of changing this symbol: {}\n\n\
            1. Call docmonk_symbol with the name. If several symbols match, list them and \
            continue with the one that fits best, saying which.\n\
            2. Call docmonk_impact on it for its callers, callees, tests and documentation.\n\
            3. Call docmonk_links with the symbol as `entity` for the specification sections \
            linked to it, and read the relevant ones with docmonk_read.\n\
            4. Report: what calls it, what it calls, which tests cover it, which documents \
            describe it, and what would have to change together. Name files and lines from \
            the results, and say what the index does not show instead of guessing.",
    },
];

/// The prompts `prompts/list` returns.
pub fn list() -> Value {
    PROMPTS
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "title": p.title,
                "description": p.description,
                "arguments": [{
                    "name": p.argument,
                    "description": p.argument_description,
                    "required": true,
                }],
            })
        })
        .collect()
}

/// `prompts/get`: the prompt's messages with its argument filled in, or
/// what is wrong with the request (an invalid-params error).
pub fn get(params: &Value) -> Result<Value, String> {
    let name = params["name"].as_str().unwrap_or_default();
    let prompt = PROMPTS
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| format!("Unknown prompt: {name}"))?;
    let argument = params["arguments"][prompt.argument]
        .as_str()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .ok_or_else(|| format!("Missing required argument: {}", prompt.argument))?;
    if argument.chars().count() > MAX_ARGUMENT_CHARS {
        return Err(format!(
            "Argument {} is longer than {MAX_ARGUMENT_CHARS} characters",
            prompt.argument
        ));
    }
    Ok(json!({
        "description": prompt.description,
        "messages": [{
            "role": "user",
            "content": {"type": "text", "text": prompt.template.replacen("{}", argument, 1)},
        }],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_prompts_are_listed_with_one_required_argument() {
        let listed = list();
        let names: Vec<&str> = listed
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["answer_with_evidence", "impact_review"]);
        for p in listed.as_array().unwrap() {
            assert_eq!(p["arguments"][0]["required"], true, "{p}");
        }
    }

    #[test]
    fn a_prompt_carries_its_argument_and_names_the_tools() {
        let got = get(&json!({"name": "answer_with_evidence",
            "arguments": {"question": "  how are refunds settled?  "}}))
        .unwrap();
        let text = got["messages"][0]["content"]["text"].as_str().unwrap();
        assert!(
            text.starts_with(
                "Answer this question from the DocMonk index: how are refunds settled?\n"
            ),
            "{text}"
        );
        for tool in ["docmonk_explore", "docmonk_evidence", "docmonk_read"] {
            assert!(text.contains(tool), "{tool}");
        }
        assert!(text.contains("insufficient_evidence"));
        let got =
            get(&json!({"name": "impact_review", "arguments": {"symbol": "Dog.bark"}})).unwrap();
        let text = got["messages"][0]["content"]["text"].as_str().unwrap();
        for tool in ["docmonk_symbol", "docmonk_impact", "docmonk_links"] {
            assert!(text.contains(tool), "{tool}");
        }
    }

    #[test]
    fn bad_requests_say_what_is_wrong() {
        assert_eq!(
            get(&json!({"name": "nope"})).unwrap_err(),
            "Unknown prompt: nope"
        );
        assert_eq!(
            get(&json!({"name": "impact_review", "arguments": {"symbol": " "}})).unwrap_err(),
            "Missing required argument: symbol"
        );
        assert_eq!(
            get(&json!({"name": "impact_review"})).unwrap_err(),
            "Missing required argument: symbol"
        );
        let long = "x".repeat(MAX_ARGUMENT_CHARS + 1);
        assert!(
            get(&json!({"name": "impact_review", "arguments": {"symbol": long}}))
                .unwrap_err()
                .contains("longer than")
        );
    }
}
