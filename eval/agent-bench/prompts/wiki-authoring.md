# Wiki Authoring Prompt

You are authoring the `.hallouminate/wiki/` knowledge base for this repository, for the wiki-grounding benchmark pilot.

## Scope

Write only repository knowledge: architecture, module boundaries, conventions, invariants, and "why this design not that one" notes — the things a new contributor would otherwise have to rediscover by reading the whole codebase.

## Hard constraints

- Do NOT include answers to any benchmark question, gold answer, or rubric note. The benchmark measures whether a *separate* grounding agent, reading only what you write here, can answer questions it has never seen. Writing an answer verbatim corrupts that measurement.
- Do NOT author, draft, or reference benchmark questions in any form.
- Write only what you would write if no benchmark existed: the wiki must reflect genuine repository knowledge, not a study guide.

These constraints keep authoring separated from the measured question set. Any wiki content that traces back to a specific benchmark question invalidates that question's grade.

## When you are done

Stop once the wiki captures the repository's architecture, conventions, and non-obvious "why" decisions. Report completion explicitly rather than continuing to add content indefinitely — the authoring budget is finite and metered.
