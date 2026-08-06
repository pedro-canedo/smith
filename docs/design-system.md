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
| `surface.base`    | `Rgb(16,18,21)`   | `Indexed(232)`    | fundo geral, texto do assistente                 |
| `surface.raised`  | `Rgb(22,24,28)`   | `Indexed(234)`    | tool cards, bolha do user, blocos de código      |
| `surface.overlay` | `Rgb(30,33,38)`   | `Indexed(235)`    | insets dentro de cards: `$ cmd`, output, diff gutter, chips de tecla |
| `surface.hover`   | `Rgb(38,42,48)`   | `Indexed(236)`    | linha selecionada (suggestions, question modal)  |

`surface.base` **é pintado** (`ui::draw` desenha um `Block` com ele sobre o
frame inteiro, e de novo sobre a área que um modal limpa com `Clear`), não
herdado do terminal. Herdar era o que tornava um tema claro impossível: os
tokens de texto foram escolhidos contra um fundo escuro que ninguém tinha
declarado, então em um terminal claro `text.primary` ficava branco no branco.

A escada de elevação do fallback 256 é mais curta que a original
(234/235/236 em vez de 234/236/238) por medição: com `hover` em `Indexed(238)`
(cinza 68), seis dos dez tokens de foreground ficavam abaixo de AA. Baixar dois
degraus custa um pouco de separação entre superfícies e devolve a paleta
inteira.

### 1.2 Texto

| Token            | Truecolor         | Fallback 256    |
| ---------------- | ----------------- | --------------- |
| `text.primary`   | `Rgb(226,229,233)`| `Indexed(253)`  |
| `text.secondary` | `Rgb(148,154,163)`| `Indexed(247)`  |
| `text.disabled`  | `Rgb(114,120,130)`| `Indexed(244)`  |

### 1.3 Semântica / roles

| Token        | Truecolor         | Fallback 256   | Uso                                        |
| ------------ | ----------------- | -------------- | ------------------------------------------ |
| `ember`      | `Rgb(255,140,60)` | `Indexed(208)` | marca, spinner, thought rows, gutter do assistente, título do input |
| `amber`      | `Rgb(255,190,90)` | `Indexed(215)` | inline code, destaque de path              |
| `success`    | `Rgb(88,206,128)` | `Indexed(84)`  | ✓, diffs `+`, teclas de confirmar          |
| `danger`     | `Rgb(243,102,102)`| `Indexed(210)` | ✗, diffs `-`, teclas de cancelar, erros    |
| `warning`    | `Rgb(250,204,21)` | `Indexed(220)` | permission modal, tools `Dangerous`        |
| `info`       | `Rgb(86,182,255)` | `Indexed(75)`  | tools read-only, slash commands, seleção   |
| `plan`       | `Rgb(198,132,255)`| `Indexed(177)` | plan mode (input, modal, sidebar)          |

### 1.4 Diff

- `diff.add`: fg `success` + bg `Rgb(24,42,30)`
- `diff.del`: fg `danger` + bg `Rgb(46,26,28)`
- `diff.hunk`/nº de linha: `text.disabled`

### 1.5 Detecção de capacidade

`Theme::detect()`: se `COLORTERM ∈ {truecolor, 24bit}` → truecolor; senão
fallback ANSI da coluna 3. Um único `Theme` é criado no `App::new` e passado
por referência para todo `draw_*` — **nenhum `Color::` literal fora de
`theme.rs`** (hoje são ~60 espalhados).

### 1.5.1 Presets, contraste e configuração

São três paletas — `dark` (o Ember das tabelas acima), `light` e
`high_contrast` — cada uma com variante truecolor e 256/16 cores. `light`
**não** é uma inversão: cada cor de papel é escolhida de novo contra fundo
claro, porque um laranja que lê como "quente" em `Rgb(255,140,60)` sobre
quase-preto é ilegível sobre quase-branco.

O que mantém as três honestas é `theme.rs::contrast_ratio` — luminância
relativa e razão de contraste da WCAG 2.1, com o cubo de 256 cores mapeado
para RGB (`indexed_rgb`) para que a variante ANSI seja mensurável, não
pulável. O teste `every_preset_meets_wcag_aa` varre **todo** par
foreground/superfície de **todo** preset e exige AA: 4.5:1 para tokens que
carregam texto, 3:1 para `text.disabled`, que só carrega cromo esmaecido
(gutters, tempos decorridos, números de linha) e nunca é o único portador de
uma informação. Quando um par falha, muda-se a cor — nunca o limiar. Foi assim
que `text.disabled` (2.42:1 sobre `hover`) e `danger` (4.34:1) mudaram de valor
no tema escuro que já existia.

`high_contrast` mira baixa visão e terminais de 16 cores: a variante ANSI é a
única que fica dentro de `Indexed(0..16)`, e nela **todas as superfícies são
pretas** de propósito — os dois fundos que as 16 cores ofereceriam (cinza 8,
azul 4) derrubam pelo menos uma cor de papel abaixo de 4.5:1, então a
elevação é trocada por legibilidade e a estrutura fica com o marcador `›` e as
bordas.

Seleção do tema, em ordem de precedência: `--theme <nome>` › `[theme]` do
`<project>/.smith/config.toml` › `[theme]` do `~/.smith/config.toml` ›
`Theme::detect()`. Overrides por token vêm de `[theme.colors]` em hex
(`ember = "#ff8c3c"`), mesclados chave a chave entre as camadas. Nome
desconhecido, token desconhecido ou hex inválido **abortam a inicialização**
com código 2 — um tema que ignora metade do que o config pediu é pior que um
que se recusa a subir.

### 1.5.2 Teclas remapeáveis (`[keys]`)

`action = "tecla"`, ex. `toggle_sidebar = "ctrl+t"`. Cinco ações:
`toggle_sidebar`, `cycle_sidebar_tab`, `toggle_logs`, `toggle_card_focus`,
`insert_newline`.

**Por que existe:** `Ctrl+B` é o prefixo padrão do tmux — o multiplexador come
a tecla antes de o smith ver o byte. `Ctrl+O` é `discard` numa disciplina de
linha termios padrão e comando de painel no screen. Não são colisões
hipotéticas: são a configuração de fábrica dos dois programas com mais chance
de estarem em volta de um agente de terminal.

**Por que só cinco.** `Enter`, `Esc`, `Backspace` e as setas ficam de fora: 
remapear o que submete, cancela e edita transforma um erro de configuração num
terminal onde não se digita nem se sai. `Ctrl+C` também fica — é checado antes
de todo o resto exatamente para sempre haver uma saída, e um arquivo que
pudesse movê-lo poderia removê-lo.

Ação desconhecida, tecla impossível de parsear, ou duas ações na mesma tecla →
erro de inicialização nomeando o culpado, nunca fallback silencioso. Duas ações
numa tecla é sempre engano, e qual das duas para de funcionar não é previsível
a partir do arquivo.

### 1.6 Glifos (tokens de caractere)

Mesma lógica das cores, para o outro eixo de capacidade do terminal.
`Theme::unicode` é detectado uma vez (`LANG`/`LC_ALL`/`LC_CTYPE` contendo
`UTF-8`, ou `SMITH_ASCII=1` para forçar o fallback) e vive **dentro do
`Theme`**, não ao lado dele — porque o `Theme` já é chave do memo do
transcript (`LayoutKey`), então trocar de conjunto de glifos invalida as linhas
renderizadas de graça, exatamente como trocar de paleta.

| Papel        | Unicode        | ASCII        |
| ------------ | -------------- | ------------ |
| spinner      | braille 10 fases | `-` `\` `\|` `/` (4 fases) |
| tool ok/erro | `✓` / `✗`      | `+` / `x`    |
| seleção      | `›`            | `>`          |
| gauge cheio/vazio | `━` / `─`  | `#` / `-`    |

Um glifo ausente na fonte do terminal não vira um caractere errado — vira uma
**célula em branco**, e um spinner que pisca para nada lê como travamento. Por
isso o fallback é por conjunto e não por caractere.

**Nenhum literal de glifo novo fora de `theme.rs`.** A regra é a irmã de
"nenhum `Color::` fora de `theme.rs`" e vale pelo mesmo motivo: o critério de
aceite #7 exige um terminal 80x24, 16 cores, sem Unicode.

> Dívida conhecida (v1): as bordas `╭╮╰╯│─`, o gutter `▌`/`▏`, a barra de
> tasks `▰▱`, os ícones de task `▶`/`◻`/`✔` e a JumpPill `↓` ainda são
> literais em `ui.rs`/`panel.rs`. São o próximo lote a migrar para os tokens
> acima; nada novo pode ser adicionado a essa lista.

### 1.7 Espaçamento e tipografia

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
- **Expandido** (seleção por card + `Enter`, ver 2.13): input completo +
  output com cap de N linhas + `… +N linhas` em `disabled`. O card selecionado
  ganha bg `hover` e o marcador `›` na primeira coluna do header.

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
Abas `Session` / `Tasks` / `Vitals` (widget `Tabs`) na primeira linha do
painel; a aba ativa em `ember` bold, as demais `disabled`, divisor = o glifo
de régua vertical do tema. Headers UPPERCASE `secondary`; valores `primary`;
barra de progresso e `▶` de task em `ember`; `PLAN MODE` chip `plan`; números
de token `primary` com breakdown `secondary`.

**Por que abas e não uma coluna só.** As quatro seções empilhadas pedem mais
linhas do que um terminal de 24 tem depois do prompt e da status bar — a 80x24
as de baixo simplesmente não existiam. A aba troca "ver tudo, truncado" por
"ver uma coisa, inteira", que é a troca certa num painel de 28 colunas. O
divisor é a régua vertical e não o `·` do tema porque ` · ` mais o espaço que
o ratatui põe de cada lado custa cinco colunas por divisor: com três abas
seriam 28 num painel de 27, e todos os títulos apareceriam cortados em quatro
letras para caber dois espaços.

Largura: os títulos completos cabem em 27 colunas; abaixo disso caem para as
quatro primeiras letras. Cortar via clipping do `Tabs` não serve — ele descarta
o último título inteiro, produzindo uma aba selecionável e invisível.

### 2.9.1 `Overlay` (painel de leitura)
`/usage` e `/mcp` como `Table` real, `Ctrl+L` como linhas. Caixa centrada,
borda `ember`, título `ember_bold`, fundo `raised`; cabeçalho de coluna
`info_bold`; rodapé `disabled` com `(scrollable)` em `warning` só quando há
o que alcançar abaixo.

**Não é uma variante de `Modal`.** Todo variante de `Modal` carrega um
`oneshot` em que o agente está bloqueado; um quarto variante deixaria `/usage`
substituir um pedido de permissão pendente e travar o turno para sempre. Vive
em campo próprio e cede a tela a qualquer modal de verdade.

### 2.10 `SlashSuggest`
Selecionada: bg `hover` + `›` + nome bold `info`; demais `secondary`; linha de
hint `disabled`. A mesma lista serve `@caminho`: muda só o prefixo impresso
(`/` ou `@`) e o padding do nome — um caminho é a entrada inteira e não tem
descrição, então alinhá-lo à coluna de um comando só empurraria tudo à
direita.

### 2.11 `JumpPill`
Se `follow_bottom == false` e chega evento novo: chip flutuante no canto
inferior direito do transcript `↓ new activity` (fg `ember` bg `overlay`);
qualquer tecla de scroll-to-bottom (`End`/scroll ao fim) dispensa.

---

### 2.12 `ContextGauge`

`LineGauge` de uma linha alimentado por `AgentEvent::ContextUsage
{ used, window, estimated }`. Cor progressiva por limiar de ocupação —
`success` < 60%, `warning` em 60–85%, `danger` ≥ 85% — sempre em tokens de
`theme.rs`. Label à esquerda do traço, com os **números reais**
(`62% 79k/128k`), porque uma razão sozinha joga fora exatamente o que o
usuário quer ler.

`estimated` não é decoração: `used` é a contagem de prompt do provider (exata)
até a última resposta, e a partir daí parte dele é um `chars/4` do delta ainda
não enviado. Um número estimado é renderizado com `~` colado nele
(`~62% 79k/128k`) **e**, onde há espaço para uma segunda linha (a sidebar),
a legenda `~ est. since last reply` em `disabled` logo abaixo. Na context
strip só o til sobrevive — uma linha é uma linha. Regra geral: nunca desenhar
estimativa com a mesma tipografia de medida.

O gauge **não anima**. Ele só muda quando chega um `ContextUsage`, e por isso
não entra em `App::is_animating()` — o loop de eventos continua redesenhando
zero vezes por segundo em repouso.

### 2.13 Foco de card e throbber por card

**Seleção.** `Ctrl+O` entra no foco de card (seleciona o card de tool mais
recente) e sai dele; `↑`/`↓` andam entre cards; `Enter` expande/recolhe o
selecionado; `Esc` sai. O transcript rola sozinho para manter o card
selecionado visível.

A seleção e a expansão são **campos do próprio `ChatLine`**, não do `App`, e
mudam através de mutadores que chamam `touch()`. É isso que as mantém fora do
`LayoutKey`: um flag de expansão global na chave invalidaria o transcript
inteiro a cada `Enter`, enquanto um stamp novo invalida exatamente um card. A
seleção sobrevive a linhas novas chegando durante o streaming porque nada é
indexado por posição — o flag anda com a linha.

**Throbber.** Cada card deriva sua fase do próprio `started_at`
(`elapsed / SPINNER_INTERVAL`), não de um contador global — dois tools que
começaram em momentos diferentes têm que girar defasados, senão a tela informa
"uma coisa está acontecendo" em vez de "estas N coisas estão acontecendo".
Cards sem `started_at` caem no contador global. Isso não custa nada ao memo:
um card `Running` já é excluído dele por `is_animating()`.

---

## 3. Layout compacto — o contrato de 80x24

Critério de aceite #7: *roda em 80x24, 16 cores, sem Unicode; tudo legível e
navegável.* Esta seção é normativa: **todo widget novo tem que se encaixar
nela antes de ser escrito**, porque adaptar depois é o que faz esse critério
falhar tarde.

### 3.1 Faixas de largura

`SIDEBAR_MIN_TERMINAL_WIDTH = 80` é o único breakpoint de largura, e 80 fica
**dentro** da faixa completa (`>=`), não fora dela — 80x24 é o alvo do
critério, então é a faixa completa que ele tem que satisfazer.

| Faixa | Largura | Sidebar | Transcript | Vitais |
| ----- | ------- | ------- | ---------- | ------ |
| completa | `w >= 80` | 28 col à direita | `min(w - 28, 100)` | na sidebar |
| compacta | `48 <= w < 80` | **não desenhada** | `min(w, 100)` | na *context strip* |
| mínima | `w < 48` | não | `w` | strip só com o gauge; status bar perde `cwd git:(branch)` e a versão |

A regra que dá sentido às três: **nada que só exista na sidebar pode ser o
único lugar onde um vital vive.** Quando a sidebar some, o que ela carregava
não some junto — desce para a *context strip*.

### 3.2 A context strip

Uma linha, entre o transcript e o input, desenhada **apenas** quando não há
sidebar e há algo para mostrar. Conteúdo, em ordem de prioridade, cortando da
direita para a esquerda conforme a largura:

1. `ContextGauge` (2.12) — o único item que sobrevive à faixa mínima;
2. `tasks 3/8`;
3. `12.4 tok/s`;
4. `~$0.0123` (ou `CPU 42%` quando o provider é local e há `ResourceStats`).

O que é **descartado** e não reaparece em lugar nenhum abaixo de 80 colunas:
o breakdown `in / out` de tokens, a lista de tasks pendentes (vira só a
contagem), e o bloco `MACHINE` completo (vira `CPU %`). São detalhes de
inspeção, não vitais de turno.

### 3.3 Orçamento vertical

Alocação por **prioridade**, não por posição — cada passo só gasta o que
sobrou do anterior:

| # | Região | Reserva |
| - | ------ | ------- |
| 1 | status bar | 1 linha, sempre |
| 2 | input | `INPUT_MIN_ROWS` (3), sempre |
| 3 | transcript | piso de 8 linhas, ou tudo que sobrar num terminal mais baixo |
| 4 | context strip | 1 linha, só se `h >= 20` e não há sidebar |
| 5 | sugestões de slash | até 6 + 1 de hint, do que sobrar — e só ela pode tomar emprestado do piso do transcript, até o piso relaxado de 4, quando não caberia de outro jeito |
| 6 | crescimento do input | até `INPUT_MAX_ROWS` (10), do que ainda sobrar |

Consequência em 80x24 com a lista de slash aberta: 1 status + 7 sugestões +
8 input + **8 transcript** — o piso, nunca menos. Sem a lista: 1 + 10 + 13.

O piso do transcript é o que essa tabela existe para proteger. A ordem antiga
(input cresce até 10 contra `height - 3`) deixava o transcript com 6 linhas
assim que o usuário digitava `/`.

### 3.4 Invariantes que valem em qualquer faixa

- **Largura:** nenhuma linha excede a largura do painel. Vale para a sidebar
  também, que por isso passa por `fit_lines` em vez de confiar no `Wrap` —
  contagem de linhas tem que ser igual a contagem de células verticais, senão
  não dá para posicionar o gauge por offset.
- **Custo em repouso:** nada que apareça em qualquer faixa pode fazer
  `App::is_animating()` virar `true`. Um vital que pisca custa 8 redraws/s
  para sempre.
- **Navegabilidade:** toda informação que a faixa completa mostra ou está na
  faixa compacta, ou é alcançável por teclado (expandir card, `/usage`).

---

## 4. Comportamento agentic (entre requisições)

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
- Verbosidade: `Ctrl+O` entra/sai do foco de card; `↑↓` andam entre cards,
  `Enter` expande o selecionado (2.13). Erros sempre mostram tail
  independente do modo.
- Nada muda em `smith-core`: `ToolCallStarted` já carrega `input` e
  `ToolCallResult` já carrega `output` — hoje o TUI **descarta** ambos
  (`app.rs:1129-1178` só gera label). O redesign é quase todo na camada TUI.

---

## 5. Plano de implementação (fases)

Gate por fase: `cargo fmt --all -- --check && cargo clippy --workspace
--all-targets -- -D warnings && cargo test --workspace`.

| # | Fase | Escopo | Risco |
| - | ---- | ------ | ----- |
| F0 | **Tokens** | `theme.rs` (struct `Theme`, `detect()`, paletas truecolor/ANSI); substituir todos os `Color::` literais de `ui.rs`/`markdown.rs`. | baixo, mecânico |
| F1 | **Primitivas** | `components/{mod,panel,chips,diff}.rs`; add `similar = { workspace = true }` em `smith-tui`. | baixo |
| F2 | **Modelo de dados** | `ChatLine` ganha `tool_name`, `input`, `output`, `duration`, `expanded`; inserção de `ThoughtRow` (timing de gaps) em `on_agent_event`; ajustar testes existentes de `app.rs`. | médio — toca testes |
| F3 | **Transcript** | reescrever `draw_messages` para compor os componentes 2.1–2.5; remover activity strip; `Ctrl+O`; `JumpPill`. | maior diff |
| F6 | **80x24** | seção 3 inteira: faixas de largura, context strip, orçamento vertical, `ContextGauge`, throbber por card, seleção por card. | médio — toca layout e memo |
| F4 | **Chrome** | input, status bar, sidebar, modais, idle screen, suggestions com tokens (2.6–2.11). | baixo |
| F5 | **Testes de render** | asserts com `TestBackend` por componente (header de card, diff, chips); snapshots de paleta. | baixo |

Ordem F0→F5 é importante: F0/F1 não mudam comportamento; F2 muda estado sem
mudar render (testes garantem); F3/F4 só consomem.

## 6. Fora de escopo (v1)

- ~~Temas alternativos / config de paleta em `~/.smith/config.toml`~~ —
  entregue; ver 1.5.1.
- Trocar de tema no meio da sessão (`/theme`). O `Theme` é chave do memo do
  transcript, então trocar é barato de renderizar; o que falta é decidir se
  a troca persiste no config.
- Syntax highlighting real de código (só tag de linguagem).
