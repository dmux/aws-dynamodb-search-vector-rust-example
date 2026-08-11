---
name: memory-curation
description: How to decide what goes into long-term memory and how to phrase it. Use whenever storing something with memory_remember, when the user says "remember this", "save that", "don't forget", when a session produced a durable decision or preference worth keeping, or when deciding whether something is worth storing at all.
---

# Curating what gets remembered

The `memory_remember` tool will store anything you hand it. Nothing in the
system prevents a store full of duplicated, contradictory, half-sentence
memories that make every later recall worse. That judgement is yours, and this
skill is the policy.

An over-full memory is not a neutral cost. Recall returns the `top_k` nearest
vectors regardless of quality, so every piece of noise competes with signal for
those slots — a store with forty near-duplicate fragments about one project
will return five fragments and crowd out the fact that would have mattered.

## The test for whether to store

Store it only if you can answer yes to **both**:

1. **Would this change my answer in a later, unrelated session?**
2. **Would it still be true then?**

That is a much higher bar than "was it mentioned". Applied honestly it rejects
most of a conversation.

| Store | Do not store |
|---|---|
| Stated preferences and working style | Anything the user asked to keep private |
| Stable facts about the user, their team, their systems | Secrets: credentials, tokens, keys, URLs with signatures |
| Decisions with lasting consequences, **and their reasoning** | The current task's transient state |
| Constraints that outlive the task ("we cannot use X because Y") | Anything reconstructible from the repo, git history, or CLAUDE.md |
| External pointers: dashboards, ticket systems, runbooks | Conclusions you inferred but the user never confirmed |

The "reconstructible" row is the one most often violated. Code structure, past
fixes, and file layout are already in the repository, and the version there
stays correct as the code changes while a memory of it silently rots. Store the
*why* that the repo does not record, not the *what* that it does.

The secrets rule is absolute and has no exception for "it's only a test key".

## Recall before you remember

**Always search before storing.** This is the single highest-value habit in
this skill, and skipping it is how stores degrade.

```
memory_recall(query: <the text you are about to store>, top_k: 5)
```

Then read the distances (cosine, lower is closer — see the `memory-recall`
skill for the bands):

| What comes back | What to do |
|---|---|
| A hit at distance ≲ 0.15 saying the same thing | **Do not store.** It is already known. |
| A hit at ≲ 0.15 that is *outdated* | `memory_forget` the old id, then store the new text. Never leave both. |
| A hit at ≲ 0.30 that is related but distinct | Store the new one, phrased so the difference is explicit. |
| Nothing under ~0.40 | Store it. |

Two memories that contradict each other are worse than either alone: recall
returns both, ranked by vector distance, which has no idea which is current.
Superseding means **forget then remember** — never stacking a correction on top.

## How to phrase a memory

The stored text is what gets embedded, and it is the *entire* context a future
session receives. It has no conversation around it.

**Write self-contained declarative sentences.**

| Bad | Why | Good |
|---|---|---|
| "prefers the second option" | No referent survives the session | "Prefers Terraform over CDK for infrastructure, for reviewability of the plan output" |
| "it's broken" | No subject, no durability | "The staging vector index was created with 512 dimensions and cannot be resized in place" |
| "Rafael likes Rust and prefers hexagonal architecture and dislikes mocking frameworks" | Three facts in one item | Three separate memories |

**One fact per memory.** Compound memories embed to the average of their
topics, so they match every one of those topics weakly and none of them well.
They also cannot be partially forgotten — correcting one third means deleting
and rewriting the whole thing.

**Phrase it the way it will be searched for.** Cosine similarity compares your
stored sentence against a future query sentence. Include the words a future
question would use. "Uses pnpm, not npm, in all JS repos" will match a query
about "package manager"; "pnpm@9" will not.

## Choosing the kind

`kind` is stored as an `INLINE_FILTER` on the vector index, so filtering by it
narrows the search at the storage layer rather than after the fact. Getting it
right makes later recalls both cheaper and sharper.

| Kind | Meaning | Example |
|---|---|---|
| `preference` | A stated taste, style, or way of working. Subjective, and true because the user says so. | "Prefers `make` targets over shell scripts for repeatable tasks" |
| `fact` | Objectively true about the user or their world. The default. | "Deploys to us-east-1; the org has no other enabled region" |
| `episode` | Something that happened, tied to a point in time. | "On 2026-08-04 chose DynamoDB vector search over OpenSearch to avoid a dual-write pipeline" |

Unspecified defaults to `fact`. If you are torn between `fact` and `episode`,
ask whether the *timing* is part of the meaning. Decisions are usually
`episode`: they were made at a moment, by someone, for reasons that may later
be revisited.

Anything that could later be revised is better as `episode` with the date in
the text than as a `fact` presented as timeless.

## Choosing a TTL

`ttl_seconds` is enforced by DynamoDB TTL, which deletes the item **and** its
vector index entry. It is the only forgetting mechanism that needs no
maintenance, and it is underused.

| Horizon | `ttl_seconds` | For |
|---|---|---|
| A sprint | `1209600` (14d) | Current focus, active workstream |
| A quarter | `7776000` (90d) | Team shape, active project constraints |
| A year | `31536000` | Employer, role, tooling generation |
| Never — omit | — | Stable personal preferences, identity, permanent decisions |

Default to omitting it for preferences and setting one for anything with the
word "currently" or "this quarter" in it. A memory that says "currently
migrating to X" with no TTL becomes actively misleading the moment the
migration finishes, and nothing will ever prompt you to remove it.

DynamoDB TTL deletion is not instant — it typically fires within 48 hours of
expiry, not at the second. Do not use it as a precise timer.

## Writing on the user's behalf

Storing something the user did not ask you to store is fine and often correct —
that is what makes memory work without ceremony. Two rules make it safe:

- **Say what you stored**, in one short line, so a wrong one can be corrected
  immediately. Not a paragraph, not a confirmation prompt.
- **Never store an inference the user did not confirm.** "Seemed frustrated
  with Terraform" is an interpretation. "Said Terraform's plan output is the
  reason they prefer it over CDK" is a report.

If the user explicitly asked you to remember something, store it as asked even
if it fails the durability test — but you may say once that it looks transient.

## What a write costs

One `memory_remember` is one Bedrock Titan embedding (1024 dimensions) plus one
DynamoDB `PutItem` that indexes the vector in the same operation. It is cheap
in money and always at least one network round trip in latency. The reason to
be selective is not the write cost — it is that every stored item permanently
competes for the `top_k` slots of every future recall.

Search is eventually consistent. A memory stored seconds ago may not appear in
the next recall; that is the index catching up, not a failed write. Do not
retry the write.
