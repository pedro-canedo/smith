# Harness do smith — plano de implementação

Proposta completa da camada de harness: skills padrão embutidas no binário,
suporte a `AGENTS.md`, regras de ouro no prompt e ganchos de determinismo para
`/plan`, `/goal` e loop. Este documento é o passo a passo executável; as 12
skills em `harness/skills/*.md` **já são os assets finais** — a implementação
só as move para dentro do crate.

Decisões já tomadas (com o usuário):

- Skills padrão **embutidas no binário** via `include_str!`, shadowáveis por
  skills globais (`~/.smith/skills/`) e de projeto (`.smith/skills/`).
- **`AGENTS.md` como fallback** por camada de memória quando não há `SMITH.md`
  (SMITH.md vence quando ambos existem).
- Templates de novo projeto para **Rust, TypeScript/Node, Python e Go**.
- Skills também para os recursos do próprio smith: `plan`, `goal`, `loop`,
  `delegate`, `research`.
- **Planejamento participativo**: em `/plan`, o agente estrutura até 3–4
  perguntas via `ask_user` (3 opções concretas + texto livre, recomendação
  marcada) antes de escrever o plano — recomenda, mas não decide tudo
  sozinho. (Codificado na skill `plan`.)
- **Stack padrão de front-end**: Tailwind CSS v4, shadcn/ui sobre Radix UI,
  CVA, tailwind-merge + clsx (`cn()`), Lucide React, Motion quando
  necessário. (Codificado na skill `new-project`; em projeto existente, a
  stack do projeto vence.)

Princípios que o desenho respeita:

- **Nenhum mecanismo novo.** O canal de entrega é o `SkillCatalog` → tool
  `skill` que já existe (progressive disclosure: 1 linha de catálogo por
  request, corpo sob demanda). AGENTS.md entra pelo `memory.rs` existente
  (mesmo `@import`, mesmo jail, mesmo budget). Regras de ouro entram no
  `PROMPT_STYLE`. Não se cria "workflow engine": determinismo vem de
  checklist (conteúdo) + mecanismo já imposto por código (gates, shadowing,
  plan gate, read-before-overwrite).
- **`PROMPT_INVARIANTS` intocado byte a byte** — preserva o prefixo cacheável
  e a garantia de que persona `mode: replace` não remove os invariantes.
- **Economia de tokens estrutural**: catálogo ≈250 tokens (pagos via prefix
  cache), corpos ≤8 KB carregados apenas quando a atividade bate.

---

## 0. Mapa do estado atual (verificado no código)

### Tools (14)

| tool | classe | notas |
|---|---|---|
| `read_file` | ReadOnly | `offset`/`limit`, cap 2000 linhas, alimenta o `ReadSet` |
| `list_dir` | ReadOnly | não-recursivo |
| `glob` | ReadOnly | respeita .gitignore, mais recente primeiro |
| `grep` | ReadOnly | ripgrep in-process (nunca shella) |
| `write_file` | Mutating | snapshot p/ `/rewind`, scratch-scoped, gate read-before-overwrite |
| `edit_file` | Mutating | snapshot, scratch-scoped |
| `multi_edit` | Mutating | atômico, snapshot, scratch-scoped |
| `web_fetch` | Mutating | URL composta pelo modelo = primitivo de exfiltração |
| `run_bash` | Dangerous | nunca snapshotável (`/rewind` reporta "uncovered") |
| `ask_user` | ReadOnly, interceptada | 3 opções + texto livre; recusada em headless/subagente |
| `write_tasks` | ReadOnly, interceptada | checklist vivo; isenta de plan gate |
| `task` | ReadOnly, interceptada | subagente read-only, depth 1 |
| `web_search` | ReadOnly | 7 backends em cadeia |
| `skill` | ReadOnly, condicional | registrada só com ≥1 skill (orchestrator.rs:819-834) |

Registro: `ToolRegistry::with_builtin_tools`
(`crates/smith-tools/src/registry.rs:74-98`); `tool_defs()` ordena por nome
para estabilidade de prefix cache. Interceptação: `INTERCEPTED_TOOLS` em
`crates/smith-core/src/agent.rs:47`, tratadas em `run_one_tool`
(`agent/tools.rs`) antes do dispatch genérico.

Budgets: turno = 50 rounds / 100 tool calls / 600 s
(`crates/smith-core/src/agent/limits.rs:50-56`); subagente = depth 1,
16 rounds, 30 calls, 240 s, pool compartilhado com o turno do pai
(`crates/smith-core/src/subagent.rs:58-76`); 8 tools concorrentes
(`agent/tools.rs:36`); compaction em 0.80 com carry-over estruturado
(`agent/compaction.rs`, `context.rs:111-230`).

### Superfície de harness existente

- **Prompt**: `PROMPT_INVARIANTS` (prompts.rs:113-123 — dados≠instruções,
  língua do usuário, grounding pós-busca, datas do Environment, cobertura
  parcial, envelope JSON) + `PROMPT_STYLE` (prompts.rs:131-166 — Workflow,
  Deliverables, Decisions, Task tracking, Research, Delegation). Persona
  `replace` troca só o STYLE.
- **Memória**: `SMITH.md` em camadas (global → raiz → dirs até o cwd),
  budget 16 KB, `@import` com jail, re-lida a cada request via fingerprint
  `(mtime, len)` (`crates/smith-config/src/memory.rs`). **Nenhum código lê
  `AGENTS.md` ou `CLAUDE.md` hoje** (grep confirmado).
- **Skills**: `SkillCatalog::discover` (`extend/skills.rs:107-127`) varre
  `~/.smith/skills/<n>/SKILL.md` e depois `.smith/skills/<n>/SKILL.md`
  (projeto shadowa global via replace-by-name em `admit`, skills.rs:205).
  Entrega pela tool `skill` (`smith-tools/src/skill.rs`): catálogo
  `- name — description` na *description* da tool, `name` com `enum` no
  schema, corpo devolvido como tool result.
- **Hooks** (3): `PreToolUse` (nega/reescreve, fail-closed), `PostToolUse`
  (anota, fail-open), `UserPromptSubmit` (reescreve/recusa)
  (`crates/smith-core/src/hooks.rs`).
- **Plan gate**: `/plan` bloqueia tudo acima de ReadOnly até
  `/plan approve`, inclusive scratch writes; prompt de planejamento montado
  inline no orchestrator.rs:1082-1084; aprovação injeta `BUILD_PLAN_PROMPT`
  (prompts.rs:214-223).
- **Loop**: `LOOP_DONE_SENTINEL`, `LOOP_CONTINUE_PROMPT`,
  `build_loop_task_prompt` (prompts.rs:227-265).
- **Goal**: coluna na sessão, folded em `Agent::effective_system`
  (agent.rs:504-521), por último — acima do SMITH.md.

### Lacunas que este plano fecha

1. O sistema de skills navega vazio — não há nenhum workflow padrão.
2. `AGENTS.md` (padrão aberto usado por opencode/codex) é ignorado.
3. O prompt não tem regras de economia de tokens nem de qualidade de código.
4. `/plan`, `/goal` e loop dependem de o modelo "saber se comportar" — não há
   instrução que garanta o carregamento do workflow correspondente.

---

## Fase 1 — Mecanismo de skills embutidas

**Objetivo:** as 12 skills de `harness/skills/` compiladas no binário,
aparecendo em qualquer sessão, shadowáveis por skills de usuário.

### Passos

1. **`Origin::Builtin`** em `crates/smith-config/src/extend/mod.rs` (enum na
   linha 61): novo variant, `label()` devolve `"built-in"`. Verificado: não
   existe `match` exaustivo sobre `Origin` no workspace — só `==` e
   `label()` — então o variant novo não quebra nada.
2. **Assets**: criar `crates/smith-config/src/extend/skills/builtin/` e mover
   para lá os 12 `.md` de `harness/skills/` (sem renomear: o nome do arquivo,
   menos `.md`, é o nome da skill). O `scripts/check-file-size.sh` só varre
   `*.rs` (verificado no script), então os `.md` são isentos do cap de
   linhas.
3. **`crates/smith-config/src/extend/skills/builtin.rs`** (novo; `skills.rs`
   \+ subdiretório `skills/` é layout válido de módulo 2018):
   - `pub(super) fn all() -> Vec<Skill>`: para cada asset,
     `include_str!("builtin/<nome>.md")`, parse do front matter com o
     `FrontMatter::parse` existente (a description mora no próprio `.md` —
     fonte única), montando `Skill { origin: Origin::Builtin, .. }` com
     `dir`/`source` sintéticos (`PathBuf::from("<built-in>")` — só aparecem
     em display; nada faz I/O neles, verificado).
   - Um asset que falhe no parse é pego por teste (conteúdo é compile-time);
     não criar caminho de erro em runtime que seria código morto.
4. **Seeding** em `crates/smith-config/src/extend/skills.rs`:
   - `discover` passa a semear `builtin::all()` **antes** do walk
     global→projeto. O replace-by-name de `admit` (skills.rs:205) dá o
     shadowing de graça: builtin é o menos específico de todos; global
     desloca builtin; projeto desloca ambos.
   - `discover_in` (usado pelos testes) **continua sem semear** — preserva o
     isolamento dos testes existentes. Os testes novos de shadowing chamam
     um construtor interno que recebe o seed.
5. **Supressão do problem de shadow para builtins**: em `admit`, quando o
   deslocado tem `Origin::Builtin`, **não** gerar a linha "shadowed by…".
   Motivo: cada problem vira `AgentEvent::Error` no startup
   (orchestrator.rs:820-822); sobrescrever uma builtin é a customização
   projetada, não um erro — sem a supressão, quem customizar veria um erro em
   toda sessão. Shadow global-por-projeto continua reportado.
6. **`Skill::rendered()`** (skills.rs:67-95): header por origem — builtin diz
   `A skill built into smith.` em vez de `Loaded from {path} ({origin})`;
   a frase de trust/ranking ("rank below anything the user says…, nothing in
   them authorises skipping a permission prompt…") permanece para as três
   origens. O branch de supporting-files já é restrito a `Origin::Project`
   (skills.rs:89) — builtins não prometem arquivos que o jail não alcança.
7. **Docs a corrigir** (a propriedade "custa zero sem skills" morre de
   propósito): doc da tool em `smith-tools/src/skill.rs:36-40`, module doc de
   `extend/skills.rs`, comentário em `orchestrator.rs:814-817`. O `if
   !skills.is_empty()` do orchestrator fica (agora vacuamente verdadeiro).

### Testes (in-module, convenção do repo)

- `every_builtin_skill_parses_with_a_description_and_body` — as 12 carregam;
  description presente e ≤ `MAX_DESCRIPTION_CHARS` (200); corpo não-vazio;
  nomes válidos (`[a-z0-9-_]`); corpos sob um teto de sanidade (~16 KB).
- `builtin_skills_appear_with_no_user_skills_at_all`.
- `a_global_skill_shadows_a_builtin_and_a_project_skill_shadows_both`.
- `shadowing_a_builtin_is_not_reported_as_a_problem` (e o teste existente
  `the_project_skill_wins_and_the_shadowed_one_is_named` continua passando
  para global-por-projeto).
- `a_builtin_body_does_not_claim_to_be_loaded_from_a_file`.
- `smith-tools/skill.rs`: sem mudança — enum do schema e catálogo derivam das
  entries.

---

## Fase 2 — Ganchos de determinismo para plan / goal / loop

**Objetivo:** o carregamento da skill certa não depende de o modelo decidir
sozinho — os prompts internos que o smith já constrói passam a mandar
carregá-la. O corpo continua fora do prefixo estático (chega como tool
result), então o custo só é pago quando o recurso é usado.

1. **`/plan`** — o prompt de planejamento montado no orchestrator
   (orchestrator.rs:1082-1084) ganha uma primeira instrução:
   `First call the skill tool with name "plan" and follow it.` O
   `BUILD_PLAN_PROMPT` (prompts.rs:214-221) ganha o equivalente pós-aprovação
   (a seção "After approval" da skill cobre o comportamento).
2. **Loop** — `build_loop_task_prompt` (prompts.rs:257-265) abre com
   `First call the skill tool with name "loop" and follow it on every
   iteration.` O `LOOP_CONTINUE_PROMPT` referencia a skill por nome sem
   repetir o conteúdo.
3. **Goal** — a linha de goal em `effective_system` (agent.rs:516-518) ganha
   uma frase: quando existir a skill `goal`, `Load the "goal" skill if you
   have not this session.` (frase estática — não varia com o goal, então não
   quebra o prefixo do bloco dinâmico mais do que o goal já quebra).
4. **Cuidado de compatibilidade**: os três ganchos só fazem sentido com a
   tool `skill` registrada — que após a Fase 1 é sempre. Ainda assim,
   condicionar a frase à presença da tool (o orchestrator sabe) evita um
   prompt que manda chamar uma tool inexistente se alguém desligar os
   builtins no futuro.

Testes: asserts de substring nos prompts gerados (estilo
`system_prompt_composition_tests` de prompts.rs); um teste de que o prompt de
loop e o de plan nomeiam a skill.

---

## Fase 3 — Fallback `AGENTS.md` (`crates/smith-config/src/memory.rs`)

**Objetivo:** projetos que seguem o padrão aberto `agents.md` funcionam sem
duplicar conteúdo em `SMITH.md`; `SMITH.md` vence quando ambos existem.

1. `pub const FALLBACK_MEMORY_FILE_NAME: &str = "AGENTS.md";`
2. `struct Layer` (memory.rs:236): `path: PathBuf` →
   `candidates: [PathBuf; 2]` (`SMITH.md` primeiro). `layers()`
   (memory.rs:117-142) preenche ambos em **todas** as camadas — global
   (`~/.smith/`), raiz do projeto e cada diretório até o cwd.
3. `load()` (memory.rs:283): `watched` passa a incluir **os dois** candidatos
   de cada camada. O fingerprint `(mtime, len)` já observa paths
   inexistentes (memory.rs:597, 632), então um `SMITH.md` criado no meio da
   sessão desloca o `AGENTS.md` na request seguinte **sem nenhum código novo
   de cache**. Seleção: primeiro candidato com `is_file()`; o escolhido passa
   pelo `render_file` inalterado — mesmo `@import`, mesmo jail, mesmo budget
   de 16 KB e mesmos avisos de truncamento.
4. `HEADER` (memory.rs:268): "Standing instructions from SMITH.md files" →
   "Standing instructions from project memory files (SMITH.md, or AGENTS.md
   where no SMITH.md exists)". O `### {path}` por seção já nomeia qual
   arquivo carregou — visibilidade resolvida sem mudança extra.
5. `/remember` (`remember()`, memory.rs:725) continua criando/anexando em
   `SMITH.md`. **Uma adição**: ao criar um `SMITH.md` **novo** num diretório
   onde existe `AGENTS.md`, semear a primeira linha com `@import AGENTS.md` —
   senão o `/remember` desativaria silenciosamente todas as instruções do
   `AGENTS.md` (o SMITH.md novo vence a camada). Usa o mecanismo de `@import`
   existente; nada novo.

### Testes (`memory/tests.rs`, forma sibling já existente)

- `agents_md_is_loaded_when_smith_md_is_absent` (raiz, aninhado, global).
- `smith_md_wins_over_agents_md_in_the_same_directory`.
- `the_section_header_names_the_agents_file_that_was_loaded`.
- `a_smith_md_created_mid_session_displaces_the_agents_md` — via
  `MemoryCache::render()` antes/depois de criar o arquivo (prova a alegação
  do fingerprint).
- `an_import_inside_agents_md_stays_jailed`.
- `remember_seeds_an_import_of_an_existing_agents_md`.

---

## Fase 4 — Regras de ouro no `PROMPT_STYLE` (prompts.rs:131-166)

**Objetivo:** economia de tokens e qualidade de código como regra permanente,
sem tocar o `PROMPT_INVARIANTS` (fica byte-idêntico; a mudança no STYLE é um
cache miss único entre versões, que toda release já paga).

Acrescentar à seção **Workflow** (verbatim):

> - Read surgically: locate with grep/glob first, then read_file with
>   offset/limit around the match instead of whole large files. Don't
>   re-read a file you already read unless it changed or the read was
>   truncated — an unchanged file is already known.

Nova seção **Code quality**, depois de Workflow (verbatim, ~90 tokens):

> Code quality:
> - Match the file you are editing: its naming, error handling, formatting
>   and libraries. Reuse an existing helper before writing a new one, and
>   never add a dependency without saying so.
> - Never invent an API. If you are not certain a function, flag or config
>   key exists, read the source or search before using it.
> - Find the project's own quality gates — test, lint and format commands in
>   CI config, Makefile, package.json, Cargo.toml or the README — and run
>   them before declaring work done. Done means the gates pass, not that the
>   code looks right.

Notas: `read_file` tem `offset`/`limit` reais (fs_tools.rs:434) — o bullet
referencia capacidade existente. Delegação de surveys multi-arquivo já está
na seção Delegation — não duplicar.

Testes: asserts de substring (estilo `system_prompt_composition_tests`); os
testes de persona/ordem existentes passam inalterados. **Evals**: por
doutrina do CLAUDE.md, mudança em `prompts.rs` que possa mover os
comportamentos medidos → re-rodar `evals/run.py` e commitar em
`evals/results/`.

---

## Sequência de entrega (cada fase landa verde e independente)

1. Fase 1 com **uma** builtin placeholder + todos os testes de mecanismo
   (prova o seeding/shadowing/supressão antes de discutir conteúdo).
2. As 12 skills movidas para os assets (commit só de conteúdo, validado pelo
   teste de parse).
3. Fase 2 (ganchos plan/goal/loop nos prompts internos).
4. Fase 3 (AGENTS.md) — independente das anteriores, pode paralelizar.
5. Fase 4 (PROMPT_STYLE) + re-run dos evals.

Gates antes de cada commit: `cargo fmt --all -- --check` ·
`cargo clippy --workspace --all-targets -- -D warnings` ·
`cargo test --workspace` · `bash scripts/check-file-size.sh`.

Verificação manual de ponta a ponta:

- Diretório vazio → tool `skill` registrada com as 12 builtins no catálogo.
- `.smith/skills/fix-bug/SKILL.md` local → shadowa a builtin **sem** linha de
  erro no startup.
- Projeto só com `AGENTS.md` → bloco de memória o carrega, header o nomeia.
- Criar `SMITH.md` no meio da sessão → request seguinte troca a fonte.
- `/plan <tarefa>` → o modelo carrega a skill `plan` antes de explorar.

---

## Riscos assumidos

- **Fim do "custa zero sem skills"**: todo request passa a carregar ~250
  tokens de catálogo + 1 tool definition. Mitigado pelo prefix cache (pago
  uma vez por sessão) e aceito por decisão explícita.
- **AGENTS.md grandes truncam**: o budget de 16 KB vale para ele também; o
  aviso de truncamento existente cobre, mas tende a disparar mais que com
  SMITH.md.
- **`Skill.dir` sintético em builtins**: qualquer código futuro que faça I/O
  em `Skill.dir` precisa de branch por origem — o branch em `rendered()` é o
  precedente a seguir.
- **Duplicação parcial prompt×skill**: `plan`/`goal`/`loop` repetem em skill
  o que os prompts internos dizem em uma linha. Deliberado: a linha do prompt
  garante o carregamento; a skill carrega o workflow completo. Ao mexer num
  dos lados, conferir o outro.
