# How hallouminate compares

Choose hallouminate when the knowledge should live with a code repository as
reviewable markdown. Choose an agent-memory or knowledge-graph system when the
primary unit is a user, conversation, agent, or evolving world model instead.

This is a comparison of product shape, not a benchmark. Features change; the
links point to upstream documentation and were last checked on 2026-07-31.

## Start with the job

| If you need… | Start with… | Why |
| --- | --- | --- |
| Architecture decisions, conventions, and gotchas committed with a repository | **hallouminate** | The wiki is ordinary markdown under the repo; the search index is derived and local. |
| A local-first personal or project knowledge base with a richer note schema and knowledge graph | [Basic Memory](https://github.com/basicmachines-co/basic-memory) | Markdown remains central, with observations, relations, project management, and optional cloud sync. |
| A minimal entity-and-relation memory example for experimenting with MCP | [`@modelcontextprotocol/server-memory`](https://github.com/modelcontextprotocol/servers/tree/main/src/memory) | A small reference server stores a JSONL knowledge graph without embeddings. |
| Application-level memory scoped to users, agents, sessions, or runs | [Mem0](https://github.com/mem0ai/mem0) | It is a memory layer and SDK/server for applications rather than a repo documentation workflow. |
| Automatic capture and recall of coding-agent sessions | [claude-mem](https://github.com/thedotmack/claude-mem) | It records session activity, compresses it, and recalls relevant context across sessions. |
| Temporal facts and relationships extracted into a queryable graph | [Graphiti](https://github.com/getzep/graphiti) | It models episodes, entities, and time-aware relationships over a graph database. |

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
