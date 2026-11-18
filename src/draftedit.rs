//! Revising a pending draft without touching the owner's edits (EMB-165).
//!
//! A draft keeps the agent's own version (`agent_body`) beside the body the
//! owner edits on the Desk tab. A revision is applied to both: to the agent
//! version always, to the owner's body only where the owner left the spot
//! alone. Afterwards the diff of `agent_body` against what was sent is the
//! owner's edits and nothing else.

/// One change a session asks for.
pub enum Change<'a> {
    /// Replace what sits under the heading named so, keeping the heading.
    Section { name: &'a str, text: &'a str },
    /// Add text after the end of the body.
    Append(&'a str),
}

/// A revision the owner's edits stand in the way of, or that names nothing.
#[derive(Debug, PartialEq)]
pub enum Refusal {
    /// The agent's own version has no such section, or several.
    Section(String),
    /// The owner changed the section the revision replaces; both texts.
    Collision { section: String, owner: String, agent: String },
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::Section(m) => m.clone(),
            Refusal::Collision { section, owner, agent } => format!(
                "the owner edited the section {section:?} after the draft was written, so replacing it would overwrite their edit; nothing was changed. \
                 The owner's text is:\n{owner}\n\nThe agent's version of it is:\n{agent}\n\nRevise another section, append, or ask the owner."
            ),
        }
    }
}

/// `(level, title)` of a Markdown heading line outside a code fence.
fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    let rest = line.get(hashes..)?;
    ((1..=6).contains(&hashes) && rest.starts_with(' ')).then(|| (hashes, rest.trim().trim_end_matches('#').trim()))
}

/// The line range of the content under the heading `name` (case-insensitive),
/// up to the next heading of the same or a higher level.
fn locate(text: &str, name: &str) -> Result<(usize, std::ops::Range<usize>), String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut fence = false;
    let mut heads = vec![];
    for (i, l) in lines.iter().enumerate() {
        if l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~") {
            fence = !fence;
        } else if !fence {
            if let Some((lv, t)) = heading(l) {
                heads.push((i, lv, t));
            }
        }
    }
    let want = name.trim().trim_start_matches('#').trim();
    let hits: Vec<_> = heads.iter().filter(|h| h.2.eq_ignore_ascii_case(want)).collect();
    let titles = || heads.iter().map(|h| h.2).collect::<Vec<_>>().join(", ");
    match hits.as_slice() {
        [] => Err(format!("the draft has no section headed {want:?}; its headings are: {}", if heads.is_empty() { "none".into() } else { titles() })),
        [(at, level, _)] => {
            let end = heads.iter().find(|h| h.0 > *at && h.1 <= *level).map_or(lines.len(), |h| h.0);
            Ok((*at, at + 1..end))
        }
        _ => Err(format!("the draft has several sections headed {want:?}; revise by appending instead")),
    }
}

fn section_text(text: &str, range: &std::ops::Range<usize>) -> String {
    text.split('\n').collect::<Vec<_>>()[range.clone()].join("\n").trim().to_string()
}

fn replace_section(text: &str, at: usize, range: std::ops::Range<usize>, new: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<&str> = lines[..=at].to_vec();
    out.extend(new.trim().split('\n'));
    if range.end < lines.len() {
        out.push("");
    }
    out.extend(&lines[range.end..]);
    out.join("\n")
}

fn append(text: &str, new: &str) -> String {
    if text.trim().is_empty() {
        new.trim().to_string()
    } else {
        format!("{}\n\n{}", text.trim_end(), new.trim())
    }
}

/// Applies a change to the agent's version and, where the owner left it
/// alone, to the owner's body. Returns `(agent_body, body)`.
pub fn revise(agent: &str, body: &str, change: &Change) -> Result<(String, String), Refusal> {
    match change {
        Change::Append(text) => Ok((append(agent, text), append(body, text))),
        Change::Section { name, text } => {
            let (a_at, a_range) = locate(agent, name).map_err(Refusal::Section)?;
            let new_agent = replace_section(agent, a_at, a_range.clone(), text);
            if agent == body {
                return Ok((new_agent.clone(), new_agent));
            }
            let agent_sec = section_text(agent, &a_range);
            let collide = |owner: String| Refusal::Collision { section: name.trim().into(), owner, agent: agent_sec.clone() };
            let Ok((b_at, b_range)) = locate(body, name) else {
                return Err(collide("(the owner removed or renamed this section)".into()));
            };
            let owner_sec = section_text(body, &b_range);
            if owner_sec != agent_sec {
                return Err(collide(owner_sec));
            }
            Ok((new_agent, replace_section(body, b_at, b_range, text)))
        }
    }
}

/// The lines the owner's version removed from and added to the agent's, in
/// order, as `- ` and `+ ` lines (a line-by-line longest-common-subsequence
/// diff); empty when the two are the same.
pub fn owner_diff(agent: &str, owner: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (agent.split('\n').collect(), owner.split('\n').collect());
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut out) = (0, 0, vec![]);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j == b.len() || (i < a.len() && lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push(format!("- {}", a[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRAFT: &str = "## Summary\nfirst\n\n## Plan\nstep one\n\n## Notes\nlast";

    fn sec<'a>(name: &'a str, text: &'a str) -> Change<'a> {
        Change::Section { name, text }
    }

    #[test]
    fn an_untouched_draft_takes_the_revision_in_both_versions() {
        let (a, b) = revise(DRAFT, DRAFT, &sec("Plan", "step one\nstep two")).unwrap();
        assert_eq!(a, b);
        assert_eq!(a, "## Summary\nfirst\n\n## Plan\nstep one\nstep two\n\n## Notes\nlast");
    }

    #[test]
    fn an_owner_edit_elsewhere_survives_and_stays_out_of_the_diff() {
        let owner = DRAFT.replace("first", "first, tightened");
        let (a, b) = revise(DRAFT, &owner, &sec("Plan", "rewritten")).unwrap();
        assert!(b.contains("first, tightened") && b.contains("rewritten") && !b.contains("step one"));
        assert!(a.contains("rewritten") && a.contains("\nfirst\n"));
        assert_eq!(owner_diff(&a, &b), "- first\n+ first, tightened");
    }

    #[test]
    fn replacing_a_section_the_owner_edited_is_refused_naming_both_texts() {
        let owner = DRAFT.replace("step one", "my step");
        let Err(Refusal::Collision { section, owner: o, agent }) = revise(DRAFT, &owner, &sec("plan", "x")) else { panic!() };
        assert_eq!((section.as_str(), o.as_str(), agent.as_str()), ("plan", "my step", "step one"));
    }

    #[test]
    fn a_section_the_owner_removed_is_a_collision_and_one_the_agent_never_had_is_not_found() {
        let owner = "## Summary\nfirst";
        assert!(matches!(revise(DRAFT, owner, &sec("Plan", "x")), Err(Refusal::Collision { .. })));
        let Err(Refusal::Section(m)) = revise(DRAFT, DRAFT, &sec("Missing", "x")) else { panic!() };
        assert!(m.contains("Summary, Plan, Notes"), "{m}");
    }

    #[test]
    fn append_goes_after_the_owners_text_and_keeps_the_diff_clean() {
        let owner = format!("{DRAFT}\nowner line");
        let (a, b) = revise(DRAFT, &owner, &Change::Append("tail")).unwrap();
        assert!(a.ends_with("last\n\ntail") && b.ends_with("owner line\n\ntail"));
        assert_eq!(owner_diff(&a, &b), "+ owner line");
    }

    #[test]
    fn the_diff_of_identical_text_is_empty_and_headings_in_fences_are_not_sections() {
        assert_eq!(owner_diff(DRAFT, DRAFT), "");
        let fenced = "## A\n```\n## Plan\n```\nz";
        assert!(matches!(revise(fenced, fenced, &sec("Plan", "x")), Err(Refusal::Section(_))));
        let two = "## A\none\n## A\ntwo";
        assert!(matches!(revise(two, two, &sec("A", "x")), Err(Refusal::Section(_))));
    }

    #[test]
    fn a_subsection_stays_inside_its_parent_section() {
        let d = "## Plan\nintro\n### Detail\nd1\n## End\ne";
        let (a, _) = revise(d, d, &sec("Plan", "new")).unwrap();
        assert_eq!(a, "## Plan\nnew\n\n## End\ne");
        let (a, _) = revise(d, d, &sec("Detail", "d2")).unwrap();
        assert_eq!(a, "## Plan\nintro\n### Detail\nd2\n\n## End\ne");
    }
}
