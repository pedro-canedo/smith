# Smith Design System — "Ember"

Spec de UI para o TUI (`smith-tui`) + plano de implementação. Referência visual:
CLIs de mercado (OpenCode, Claude Code, Gemini CLI) — transcript em blocos
separados, tool calls em painéis com fundo elevado, diffs coloridos,
indicadores de "thought" recolhíveis e chrome modal por modo.

Metáfora da marca: **smith = forja**. Fundo = ferro escuro; acento = brasa
(ember/âmbar). Tudo que é "ativo" brilha; tudo que é histórico esfria (dims).

---

## 1. Tokens

### 1.1 Superfícies (elevação)

Hierarquia de fundo é o coração do redesign — hoje tudo é desenhado sobre o
fundo default do terminal, sem nenhuma diferenciação.

O fallback **não** usa as 16 cores ANSI nomeadas: com elas as três superfícies
colapsam todas em `Black` (elevação zero), e o mapeamento `DarkGray`/`Gray`
colide com os tokens de texto — `text.disabled` já é `DarkGray`, então ficaria
invisível sobre um inset `overlay`. Usamos o cubo de 256 cores, suportado bem
mais universalmente que truecolor.

| Token             | Truecolor         | Fallback 256      | Uso                                              |
| ----------------- | ----------------- | ----------------- | ------------------------------------------------ |
| `surface.base`    | default do term   | default           | fundo geral, texto do assistente                 |
| `surface.raised`  | `Rgb(22,24,28)`   | `Indexed(234)`    | tool cards, bolha do user, blocos de código      |
| `surface.overlay` | `Rgb(30,33,38)`   | `Indexed(236)`    | insets dentro de cards: `$ cmd`, output, diff gutter, chips de tecla |
| `surface.hover`   | `Rgb(38,42,48)`   | `Indexed(238)`    | linha selecionada (suggestions, question modal)  |

### 1.2 Texto

| Token            | Truecolor         | Fallback 256    |
| ---------------- | ----------------- | --------------- |
| `text.primary`   | `Rgb(226,229,233)`| `Indexed(253)`  |
| `text.secondary` | `Rgb(148,154,163)`| `Indexed(246)`  |
| `text.disabled`  | `Rgb(94,100,110)` | `Indexed(242)`  |

### 1.3 Semântica / roles

| Token        | Truecolor         | Fallback 256   | Uso                                        |
| ------------ | ----------------- | -------------- | ------------------------------------------ |
| `ember`      | `Rgb(255,140,60)` | `Indexed(208)` | marca, spinner, thought rows, gutter do assistente, título do input |
| `amber`      | `Rgb(255,190,90)` | `Indexed(215)` | inline code, destaque de path              |
| `success`    | `Rgb(88,206,128)` | `Indexed(78)`  | ✓, diffs `+`, teclas de confirmar          |
| `danger`     | `Rgb(240,90,90)`  | `Indexed(203)` | ✗, diffs `-`, teclas de cancelar, erros    |
| `warning`    | `Rgb(250,204,21)` | `Indexed(220)` | permission modal, tools `Dangerous`        |
| `info`       | `Rgb(86,182,255)` | `Indexed(75)`  | tools read-only, slash commands, seleção   |
| `plan`       | `Rgb(198,132,255)`| `Indexed(141)` | plan mode (input, modal, sidebar)          |

### 1.4 Diff

- `diff.add`: fg `success` + bg `Rgb(24,42,30)`
- `diff.del`: fg `danger` + bg `Rgb(46,26,28)`
- `diff.hunk`/nº de linha: `text.disabled`

### 1.5 Detecção de capacidade

`Theme::detect()`: se `COLORTERM ∈ {truecolor, 24bit}` → truecolor; senão
fallback ANSI da coluna 3. Um único `Theme` é criado no `App::new` e passado
por referência para todo `draw_*` — **nenhum `Color::` literal fora de
`theme.rs`** (hoje são ~60 espalhados).

### 1.6 Espaçamento e tipografia

- Largura máx. de conteúdo: 100 colunas (já existe — manter `clamp_width`).
- 1 linha em branco entre blocos do transcript; 2 antes de bolha de user.
- Painéis: padding interno de 1 coluna **de cada lado** (simétrico); bordas
  arredondadas `╭╮╰╯` — os quatro cantos, sempre — só para bolhas e modais;
  **cards usam fundo elevado sem borda** (como na referência) — borda só em
  estado de erro (gutter `▌` danger).
- **Invariante de largura:** todo construtor de caixa (`bordered_row`,
  `rounded_box`, `inset`) devolve linhas de exatamente `width` células,
  truncando o conteúdo se preciso, e `draw_messages` passa o transcript inteiro
  por `fit_lines` antes do `Paragraph`. Sem isso o `Wrap { trim: false }`
  externo dobra a linha e joga a borda de fechamento para a linha seguinte —
  a causa raiz dos desalinhamentos. `fill_line` é exceção deliberada: é a
  primitiva de "pintar fundo até a largura", com consumidores (status bar,
  header de card) que não devem ser truncados.
- Labels de seção: UPPERCASE `text.secondary` bold ("SESSION", "CONTEXT").
- Meta/caption: `text.secondary`, ex. `anthropic · claude-sonnet · 4.2s`.
- Chips de tecla: `[y]` fg semântico + bg `surface.overlay`.

---

## 2. Componentes

Novo módulo `crates/smith-tui/src/components/` — cada componente é uma fn
pura `(theme, estado, area) -> Vec<Line>` ou widget, testável com
`ratatui::backend::TestBackend`.

### 2.1 `UserBubble`
Borda arredondada `ember` + fill `surface.raised`, texto `primary`, label
`You` embutido na régua superior (`╭─ You ─────╮`). Ocupa a **largura cheia**
do painel — na mesma grade do texto do agente. Dimensionar pelo conteúdo
deixava mensagens curtas como um toco colado à esquerda, e o conteúdo era
quebrado em `width - 4` **antes** de montar a caixa (senão as linhas saem mais
largas que a moldura e o `Wrap` externo dobra a borda de fechamento).

### 2.2 `AssistantText`
Sem caixa: gutter `▌ ` (`ember`) na primeira linha e `▏ ` (`disabled`) nas
continuações, markdown sobre `base` à direita dele, com o caption de meta
(`ollama · modelo · 2.2s`) dentro do mesmo gutter. O texto em streaming usa
chrome idêntico, para não pular de posição quando o turno fecha.

Como `tui_markdown::Options` não tem parâmetro de largura nem de indentação,
o gutter é aplicado **pós-render**, sobre o `Vec<Line>` devolvido por
`markdown::render`.

Restilização de `markdown.rs`: headings bold `primary` com prefixo `#`;
inline code `amber` sobre `overlay`. **Adiado:** fenced code como painel
`raised` com tag de linguagem — `code_block_fence()` devolve só uma string de
marcador e o `StyleSheet` não tem hook por bloco, então a única saída seria
casar linhas de cerca no `Vec<Line>` e reempacotar, o que quebra com código
aninhado ou indentado.

### 2.3 `ToolCard` (o componente central)
Estados: `Running | Done | Error | Cancelled`.

- **Header (1 linha, sempre visível):** `✓/✗/spinner` colorido + nome da tool
  bold `primary` + resumo do alvo (`path`, `command`, `query`) em `secondary`
  + duração à direita em `disabled`.
- **Running:** corpo expandido sobre `raised`: para `run_bash`, inset
  `overlay` com `$ <cmd>` + tail das últimas ~6 linhas de output (novo:
  precisamos reter output incremental — v1 pode mostrar só o cmd + elapsed);
  para file tools, o path em `amber`.
- **Done:** auto-recolhe só para o header (comportamento de mercado — o
  transcript vira um sumário rolável de passos).
- **Error:** gutter `▌` danger + tail do output truncado a 3 linhas visível.
- **Expandido** (toggle `Ctrl+O` global em v1): input completo + output com
  cap de N linhas + `… +N linhas` em `disabled`.

### 2.4 `ThoughtRow`
`+ Thought: 1.1s` em `ember`, 1 linha, entre tool cards — mede o gap entre o
fim de um `ToolCallResult`/stream e o próximo evento de atividade. Recolhível
pelo mesmo toggle de verbosidade.

### 2.5 `DiffBlock`
Para `edit_file` expandido: render de `old_string → new_string` via crate
`similar` (já está no workspace, hoje só em `smith-tools` — adicionar a
`smith-tui`). Nº de linha `disabled`, linhas `-`/`+` com os tokens de diff.

### 2.6 Modais (`Permission`, `Plan`, `Question`)
Chrome comum: `Clear` + painel `raised` + borda arredondada na cor do role
(`warning` / `plan` / `info`), título bold na mesma cor, corpo `primary`,
detalhe de comando em inset `overlay`, rodapé de chips de tecla. Seleção com
bg `hover` + marcador `›`.

### 2.7 `InputChrome`
Borda por modo: idle `text.disabled` · plan `plan` · aguardando permissão
`warning`. Título ` smith ` bold `ember` sobre `overlay`; lado direito do
título: `provider/model` em `disabled`. Placeholder `disabled`; texto começando
com `/` em `info`.

Comportamento (via `components/input.rs`, casca sobre `tui-textarea-2`):
soft wrap por palavra com fallback para grafema; a caixa cresce de 3 até 10
linhas (bordas incluídas) e depois rola mantendo o caret à vista; caret real do
terminal via `frame.set_cursor_position` (não uma célula pintada); navegação
completa (setas, palavra, `Home`/`End`, `Ctrl+A/E/W/U`); `Alt+Enter` e `Ctrl+J`
inserem nova linha (`Shift+Enter` só onde o protocolo kitty estiver ativo);
paste bracketed preserva `\n` em vez de submeter na primeira quebra.

### 2.8 `StatusBar` (footer, 1 linha, bg `raised`)
Esquerda: `cwd git:(branch)` `secondary` · centro: fase + spinner `ember`
quando ocupado · direita: `v0.1.0 · ~$0.0123 (est.)` `disabled`.

### 2.9 `Sidebar`
Headers UPPERCASE `secondary`; valores `primary`; barra de progresso e
`▶` de task em `ember`; `PLAN MODE` chip `plan`; números de token `primary`
com breakdown `secondary`.

### 2.10 `SlashSuggest`
Selecionada: bg `hover` + `›` + nome bold `info`; demais `secondary`; linha de
hint `disabled`.

### 2.11 `JumpPill`
Se `follow_bottom == false` e chega evento novo: chip flutuante no canto
inferior direito do transcript `↓ new activity` (fg `ember` bg `overlay`);
qualquer tecla de scroll-to-bottom (`End`/scroll ao fim) dispensa.

---

## 3. Comportamento agentic (entre requisições)

Ciclo de um turn, e o que a tela mostra em cada estado:

1. **idle** → user envia → **thinking**: `ThoughtRow` com spinner ember no
   fim do transcript + status bar ocupada.
2. **streaming**: markdown renderizado live sobre `base` (como hoje).
3. **tool call**: `ToolCard` abre em `Running` (a activity strip atual some —
   vira redundante; o card *é* a activity). Permissão → modal `warning`; ao
   aprovar, card continua.
4. **resultado**: card vira `Done` e recolhe para o header; erro mantém tail
   visível. Volta para **thinking** (nova `ThoughtRow`) até resposta final.
5. **final**: assistant text + meta caption; status bar volta a idle;
   `follow_bottom` restaura o foco no fim.

Regras:
- Auto-follow de scroll mantido; `JumpPill` quando o user está lendo histórico.
- `Esc` cancela: cards `Running` viram `Cancelled` (ícone `·` `disabled`).
- Verbosidade: `Ctrl+O` alterna compacto/expandido globalmente; erros sempre
  mostram tail independente do modo.
- Nada muda em `smith-core`: `ToolCallStarted` já carrega `input` e
  `ToolCallResult` já carrega `output` — hoje o TUI **descarta** ambos
  (`app.rs:1129-1178` só gera label). O redesign é quase todo na camada TUI.

---

## 4. Plano de implementação (fases)

Gate por fase: `cargo fmt --all -- --check && cargo clippy --workspace
--all-targets -- -D warnings && cargo test --workspace`.

| # | Fase | Escopo | Risco |
| - | ---- | ------ | ----- |
| F0 | **Tokens** | `theme.rs` (struct `Theme`, `detect()`, paletas truecolor/ANSI); substituir todos os `Color::` literais de `ui.rs`/`markdown.rs`. | baixo, mecânico |
| F1 | **Primitivas** | `components/{mod,panel,chips,diff}.rs`; add `similar = { workspace = true }` em `smith-tui`. | baixo |
| F2 | **Modelo de dados** | `ChatLine` ganha `tool_name`, `input`, `output`, `duration`, `expanded`; inserção de `ThoughtRow` (timing de gaps) em `on_agent_event`; ajustar testes existentes de `app.rs`. | médio — toca testes |
| F3 | **Transcript** | reescrever `draw_messages` para compor os componentes 2.1–2.5; remover activity strip; `Ctrl+O`; `JumpPill`. | maior diff |
| F4 | **Chrome** | input, status bar, sidebar, modais, idle screen, suggestions com tokens (2.6–2.11). | baixo |
| F5 | **Testes de render** | asserts com `TestBackend` por componente (header de card, diff, chips); snapshots de paleta. | baixo |

Ordem F0→F5 é importante: F0/F1 não mudam comportamento; F2 muda estado sem
mudar render (testes garantem); F3/F4 só consomem.

## 5. Fora de escopo (v1)

- Temas alternativos / config de paleta em `~/.smith/config.toml` (a struct
  `Theme` já deixa a porta aberta).
- Expansão por-card com navegação de foco (v1 = toggle global).
- Syntax highlighting real de código (só tag de linguagem).
