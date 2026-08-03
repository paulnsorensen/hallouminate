---
title: "How it compares"
---

Choose hallouminate when the knowledge should live with a code repository as
reviewable markdown. Choose an agent-memory or knowledge-graph system when the
primary unit is a user, conversation, agent, or evolving world model instead.

This is a comparison of product shape, not a benchmark. Features change; the
links point to upstream documentation and were last checked on 2026-08-01.

## Start with the job

| If you need… | Start with… | Why |
| --- | --- | --- |
| Architecture decisions, conventions, and gotchas committed with a repository | **hallouminate** | The wiki is ordinary markdown under the repo; the search index is derived and local. |
| A local-first personal or project knowledge base with a richer note schema and knowledge graph | [Basic Memory](https://github.com/basicmachines-co/basic-memory) | Markdown remains central, with observations, relations, project management, and optional cloud sync. |
| A minimal entity-and-relation memory example for experimenting with MCP | [`@modelcontextprotocol/server-memory`](https://github.com/modelcontextprotocol/servers/tree/main/src/memory) | A small reference server stores a JSONL knowledge graph without embeddings. |
| Application-level memory scoped to users, agents, sessions, or runs | [Mem0](https://github.com/mem0ai/mem0) | It is a memory layer and SDK/server for applications rather than a repo documentation workflow. |
| Automatic capture and recall of coding-agent sessions | [claude-mem](https://github.com/thedotmack/claude-mem) | It records session activity, compresses it, and recalls relevant context across sessions. |
| Temporal facts and relationships extracted into a queryable graph | [Graphiti](https://github.com/getzep/graphiti) | It models episodes, entities, and time-aware relationships over a graph database. |
| A code/document graph derived automatically from existing artifacts | [graphify](https://github.com/Graphify-Labs/graphify) | It parses code, docs, and media into typed nodes and confidence-labeled edges, traversed from lexical seed matches. |
| Automatic observation capture through agent lifecycle hooks | [agentmemory](https://github.com/rohitg00/agentmemory) | It records what the agent did via hooks, compresses it into observations, and serves them back over a large MCP tool surface. |

## hallouminate and Basic Memory

Basic Memory is the closest match in this list. Both are local-first, expose
MCP tools, and keep human-readable markdown as durable data. The difference is
where each product draws its boundary:

- **hallouminate is repository infrastructure.** A wiki is conventionally
  stored at `.hallouminate/wiki/`, can be reviewed in the same pull request as
  the code, and is searched through repo-aware corpora. Its markdown has no
  required note schema.
- **Basic Memory is a knowledge-management system.** Its markdown format adds
  entities, observations, relations, and frontmatter; it supports multiple
  projects and optional cloud sync. Its current local search also includes
  semantic search, so “hallouminate has vectors while Basic Memory has only
  full-text search” is not a meaningful distinction.

The practical choice is therefore workflow and scope, not a claim that one
search stack is universally better.

## hallouminate and graphify

graphify sits on the opposite side of the same arrow: it *derives* a map from
artifacts that already exist, while hallouminate *stores* knowledge that
exists nowhere else. graphify parses source, docs, and media into a typed
graph committed inside the repo (`graphify-out/graph.json`) and answers
queries by lexically matching node labels, then traversing edges. It gives
you structure on day zero with no authoring effort, but its ceiling is the
artifacts' ceiling: it cannot hold "we tried X and it failed because Y"
unless someone already wrote that down — which is exactly the fact class
hallouminate exists for.

The retrieval trade is mirror-image: graphify has structural expansion but no
semantic matching at query time; hallouminate has hybrid semantic retrieval
but only one-hop `[[wikilink]]` traversal via `backlinks`. The two compose
rather than compete — graphify's `--wiki` output is ordinary markdown that a
`[[corpus]]` entry can index, so union search can cover a derived map and a
hand-authored wiki side by side, with per-chunk corpus provenance telling the
agent which is which.

## hallouminate and agentmemory

agentmemory shares a tagline but not a category: it is an observational
memory system. Fourteen lifecycle hooks auto-capture agent activity into a
key-value store outside the repository (`~/Library/Application
Support/agentmemory`), compressed into structured observations and served
through roughly fifty MCP tools alongside an `iii` engine process, REST,
stream, and viewer ports. One system remembers *what happened*; hallouminate
retrieves *what was decided*.

The fault line is source of truth. agentmemory's index is authoritative —
losing it loses the memories — and its records are machine-generated
artifacts inspected through a dashboard. hallouminate's markdown is the
authoritative data: reviewable in a pull request, merged by git, portable to
any tool, with a disposable index rebuilt on demand. Both independently
landed on hybrid RRF retrieval with optional cross-encoder rerank, so the
practical difference is not search quality but what is being searched and
who vouched for it.

## What hallouminate deliberately does not do

- It does not capture every agent action or conversation automatically.
- It does not decide which facts deserve to become durable documentation; the
  agent or human author does.
- It does not extract a temporal knowledge graph from conversations or events.
- It does not provide symbol, type, or call-graph analysis for source code.
- It does not require a hosted account or make cloud sync part of the storage
  model.

These constraints keep the contract small: markdown is authoritative, the
index is disposable, and a repository can carry its own durable knowledge
without depending on one agent vendor.
