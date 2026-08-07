---
description: Pick a stack for a new service: scenario defaults for language, framework, database, messaging, infra.
---

# How to use this reference

1. Identify the scenario with the user before naming technologies. These
   defaults are recommendations to OFFER — via `ask_user`, recommendation
   first and marked "(recommended)", with honest alternatives — never to
   impose. The user's choice always wins (plan skill, step 4).
2. Once the scenario is settled, propose its default below; adjust the
   pieces the user contests.
3. Then scaffold with the `new-project` skill, carrying the choices in.
4. This reflects the market as of 2026. If a choice is contested, or enough
   time has passed that a ranking could have moved, verify with
   `web_search` instead of assuming the table still holds.

# Scenario defaults

- **Corporate product** — Next.js/React + Tailwind + shadcn/ui frontend;
  **ASP.NET Core or Go** backend; PostgreSQL; Redis; Kafka; OpenTelemetry +
  Grafana + Prometheus; Docker + Kubernetes + GitHub Actions.
- **Startup / SaaS** — Next.js; **Node/TypeScript + Fastify**; Prisma;
  PostgreSQL; Redis; Docker.
- **Highest-performance API** — **Rust + Axum** + SQLx; Redis; Kafka;
  OpenTelemetry; PostgreSQL.
- **AI / agents** — **Python + FastAPI**; Ray + PyTorch; Redis; Kafka;
  PostgreSQL; MinIO.

# Backend language ranking (2026)

No language wins everything — the criteria are throughput, latency,
productivity and ecosystem, and they pull apart:

1. **Rust + Axum** — top throughput/latency/memory, zero GC, memory safety;
   moderate productivity. High-scale APIs, AI infra, gateways, proxies,
   financial systems.
2. **Go + Gin/Fiber** — the all-rounder: top marks across the board, fast
   compiles, goroutines, trivial deploys. APIs, microservices, Kubernetes
   tooling, DevOps.
3. **C# + ASP.NET Core** — heavily optimized runtime, excellent GC, great
   cloud support, top productivity.
4. **Java 21 + Spring Boot 3** — virtual threads closed the concurrency
   gap; the deepest enterprise ecosystem.
5. **Node + Fastify** — top productivity and startup speed. Never start a
   new project on Express; Fastify is superior.
6. **Python + FastAPI** — top productivity and the AI ecosystem; weakest
   raw throughput of the mainstream picks.
7. **PHP 8.4 + Laravel Octane** — Octane (Swoole/RoadRunner) changed
   Laravel's performance story.
8. **C++23 + Drogon** — extreme performance, expensive productivity.
9. **C + libhv/libevent/libuv** — only for extremely critical systems (the
   nginx/Redis/HAProxy tier).

# Per-language default stacks

- **Rust**: Axum (or Actix Web) · Tokio · Tower · Serde · SQLx (first) or
  SeaORM · Redis · Kafka · OpenTelemetry
- **Go**: Fiber or Gin (Echo acceptable) · GORM or Ent · Redis · Kafka ·
  gRPC · Zap
- **C#**: .NET 10 · ASP.NET Core Minimal APIs · EF Core · MediatR ·
  FluentValidation · OpenTelemetry · architecture: Vertical Slice + Clean
- **Java**: Java 21 · Spring Boot 3 · Spring Security · Hibernate / Spring
  Data · Virtual Threads · Micrometer
- **Node**: Node 24+ · TypeScript · Fastify · Prisma or Drizzle · Redis ·
  BullMQ · Zod · OpenTelemetry · layering: routes → controllers → services
  → repositories
- **Python**: 3.14 · FastAPI · Pydantic · SQLAlchemy 2 + Alembic · Redis ·
  Celery · OpenTelemetry — for AI add Ray · PyTorch · Kafka
- **PHP**: 8.4 · Laravel 12 + Octane (Swoole or RoadRunner) · Redis ·
  Horizon
- **C++**: C++23 · Drogon · Boost.Asio · spdlog · fmt · Redis++ · libpqxx

# Cross-cutting defaults

- **Relational DB**: PostgreSQL first. MySQL/MariaDB/SQL Server when the
  environment imposes them.
- **NoSQL**: Redis (cache, streams); MongoDB for documents; ScyllaDB or
  Cassandra for wide-column scale.
- **Search**: OpenSearch or Elasticsearch; Meilisearch for lightweight
  product search.
- **Messaging**: Kafka as the streaming backbone; NATS when lightweight;
  RabbitMQ for classic queues; Redis Streams when Redis is already there.
- **Cache**: Redis; DragonflyDB and Valkey are drop-in alternatives.
- **Observability**: OpenTelemetry always, from day one; Grafana +
  Prometheus + Loki + Tempo or Jaeger.
- **Auth**: OAuth2 / OpenID Connect with JWT; Keycloak self-hosted, Auth0
  managed.
- **API styles**: REST by default; gRPC service-to-service; GraphQL for
  aggregation-heavy frontends; WebSocket/SSE for push.
- **Containers**: Docker + Compose for dev; Kubernetes + Helm at scale.

# Architecture patterns

Start with the simplest shape that fits — a vertical slice with clear
layering. Clean/Hexagonal Architecture, DDD, CQRS, Outbox, Saga, Mediator
and friends are tools for problems you can NAME (multiple bounded contexts,
audited writes, distributed transactions) — adopting them by default is how
a CRUD service grows five layers of indirection. When one is warranted, say
which problem it solves in the plan.

# Guardrails

- Market defaults, not laws: the team's expertise and an existing
  codebase's stack outrank any ranking here.
- Never pick a stack silently. The stack is a plan-level decision the user
  participates in — surface it as a question with options, not as a fait
  accompli in step 1 of the plan.
- Do not mix scenario defaults arbitrarily: each column above is coherent
  end to end (e.g. Fastify+Prisma, not Fastify+EF Core). Deviations are
  fine when chosen, incoherences are not.
