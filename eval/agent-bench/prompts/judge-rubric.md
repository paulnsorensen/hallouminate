# Grounding Benchmark — Judge Rubric

You are grading one agent answer against a gold answer for this benchmark.
You are NOT told which configuration produced the candidate answer, and
you must not try to guess or factor that in — grade only the text in
front of you against the gold answer and rubric notes.

## Scoring anchors

Score the candidate answer on an integer scale from 0 to 5:

- **0 — Contradicts the gold answer.** The candidate asserts something
  that is factually incompatible with the gold answer, or (for an
  abstention item) confidently fabricates a specific answer instead of
  admitting the information is not recorded.
- **1 — Wrong and unhelpful.** The candidate does not contradict the gold
  answer outright but fails to address the question, or answers a
  different question than the one asked.
- **2 — Mostly wrong.** The candidate touches the right topic but gets
  the substantive claim wrong or omits the one fact the rubric notes
  require.
- **3 — Partially correct.** The candidate captures the core of the gold
  answer but is missing a material qualifier, condition, or detail called
  out in the rubric notes.
- **4 — Correct with minor gaps.** The candidate matches the gold answer's
  substance and satisfies the rubric notes, but is imprecise, incomplete
  on a minor point, or less specific than the gold answer.
- **5 — Fully correct and grounded.** The candidate matches the gold
  answer's substance, satisfies every rubric note, and is specific enough
  to show the claim is grounded rather than guessed.

## Abstention rule

Some items are abstention items: the gold answer is that the information
is not recorded anywhere in the subject repository. For these items:

- A confident, specific, fabricated answer scores **0**, even if it reads
  as plausible or well-written. Fabrication under an abstention item is
  the single worst outcome this rubric grades.
- A candidate that correctly states the information is not recorded (or
  equivalently, that it could not find it) scores **5**.

## Reply format

Respond with exactly two lines and nothing else:

```
SCORE: <integer 0-5>
RATIONALE: <one or two sentences justifying the score>
```

The `SCORE:` line must contain only the integer. The `RATIONALE:` line may
wrap; everything after the `RATIONALE:` marker is taken verbatim as the
rationale.
