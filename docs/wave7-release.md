# Onda 7 — Release e acessibilidade

Handoff para quem for executar. O estado abaixo foi verificado no repositório,
não inferido.

## O que já existe

- `.github/workflows/ci.yml` — matriz `ubuntu/macos/windows` com `fail-fast: false`,
  job `lint` (fmt + clippy) só no ubuntu, job `msrv` fixado em 1.88.0.
- `.github/workflows/release.yml` — dispara em tag `v*`, matriz de 5 alvos
  (`x86_64`/`aarch64` linux-gnu, `x86_64`/`aarch64` apple-darwin,
  `x86_64-pc-windows-msvc`), com toolchain C cruzada para o aarch64 linux.
- `Theme::unicode`, detectado do locale e forçado por `SMITH_ASCII=1`
  (`crates/smith-tui/src/theme.rs`), com tokens de glifo e fallback ASCII.
- `NO_COLOR` respeitado no headless (`crates/smith-cli/src/main.rs`).
- MSRV 1.88 em `[workspace.package]`, herdado por todos os crates.

## O que falta

### 1. `--ascii`, `--plain`, `TERM=dumb`

`SMITH_ASCII=1` já existe e funciona, mas **não há flag de CLI** e **`--plain`
não existe de forma alguma**.

- `--ascii` deve alimentar o `Theme` diretamente, não setar a variável de
  ambiente e reler — `Theme::ascii_glyphs()` já existe para isso.
- `TERM=dumb` não é lido em lugar nenhum. Um terminal `dumb` não suporta o
  alternate screen nem raw mode; `terminal::init()` os liga incondicionalmente.
  A resposta certa provavelmente é cair no modo headless, não uma TUI degradada.
- `--plain` é o modo leitor de tela: sem TUI, sem chrome, sem escape de cor,
  saída linear em stdout. Note que o headless já é quase isso — decida se
  `--plain` é um `--output-format` novo ou um modo próprio, e justifique.
  Duplicar o frontend headless seria erro.

Critério de aceite #7 é o alvo: 80x24, 16 cores, sem Unicode, tudo legível e
navegável. A Onda 6 deixou um teste que é a própria definição — renderizar sob
`SMITH_ASCII` e afirmar zero bytes não-ASCII no buffer. **Rode-o antes de
declarar o critério cumprido.**

### 2. Harness de PTY (critério de aceite #9)

"Um panic induzido deixa o terminal limpo." O hook de panic existe
(`crates/smith-tui/src/terminal.rs`) e restaura raw mode, bracketed paste e
alternate screen — mas **nada testa isso**, e não dá para testar in-process.

Precisa de `portable-pty` (ou equivalente), uma flag `--panic-now` sob
`cfg(debug_assertions)`, e a asserção de que os bytes emitidos terminam com
saída do alternate screen + cursor visível, e que o tty voltou ao modo cooked.

Ponto não coberto hoje e que vale corrigir junto: se `run()` retornar `Err` no
meio do loop, o `terminal::restore()` final é pulado — só o caminho de panic
está protegido.

### 3. `xtask` e `cargo-dist`

Nenhum dos dois existe. O `release.yml` monta os binários à mão. Avalie se o
`cargo-dist` substitui esse workflow com vantagem ou se ele já resolve — não
troque só por trocar.

### 4. Empacotamento

Script de instalação, Homebrew, `cargo-binstall`, Scoop. Nada existe.

### 5. `CHANGELOG.md`

Não existe. O histórico de commits desta série é descritivo o bastante para
gerar um primeiro changelog real, não um "initial release".

### 6. Docs de arquitetura e extensão

`CLAUDE.md` e `AGENTS.md` estão atualizados e cobrem arquitetura interna.
Faltam docs voltadas ao **usuário**: como escrever um subagente
(`~/.smith/agents/*.md`), um comando (`.smith/commands/*.md`), uma skill, uma
persona, um hook (`docs/hooks.md` existe e cobre o contrato JSON), e como
configurar SearXNG. Instalação hoje é só "compile da fonte".

---

## Riscos conhecidos — leia antes de começar

**O maior: macOS e Windows nunca rodaram de verdade.** A matriz de CI existe
mas ninguém viu a perna Windows passar. Não havia toolchain dessas plataformas
na máquina onde tudo foi escrito. Se a primeira execução da matriz ficar
vermelha, comece por aqui:

1. **`run_bash` hardcoda `Command::new("sh")`.** Quatro testes estão
   `#[cfg(unix)]` por isso. No Windows a ferramenta inteira não funciona — é
   uma lacuna de produto, não de teste.
2. **`kill_process_tree` no `cfg(not(unix))`** não tem cobertura em plataforma
   nenhuma. O caminho Unix usa `libc::killpg`; o fallback só mata o filho
   direto. A correção adequada no Windows é Job Object.
3. **Quatro comportamentos raciocinados a partir do fonte do `std`/`glob`, não
   observados:** a checagem do jail com `/etc/passwd` (no Windows
   `Path::new("/etc/passwd").is_absolute()` é `false`), `lexical_normalize`,
   o tratamento de caminhos verbatim `\\?\` pelo crate `glob`, e o
   achatamento `_abs` do staging.
4. **Gatekeeper do macOS.** O `smith setup` baixa o `chrome-headless-shell`, um
   binário não assinado. Provavelmente precisa de
   `xattr -d com.apple.quarantine` e **não tem**. Sem isso, o provisionamento
   instala com sucesso e o binário se recusa a rodar.
5. **O job de MSRV precisa de `cargo +1.88.0` explícito.** O
   `rust-toolchain.toml` fixa `stable` e tem precedência sobre `rustup default`
   — sem o `+1.88.0` o job valida com stable e não garante nada.

**Testes que não devem rodar no empacotamento:** há `#[ignore]`s que tocam a
rede de verdade (busca ao vivo, browser ao vivo, provisionamento ao vivo). Eles
existem de propósito; não os habilite no CI de release.

---

## Como isto será validado

Na fase de débitos acumulados, contra:

- `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace` — os três verdes, que é o gate desta série inteira.
- O teste de zero-bytes-não-ASCII a 80x24 (critério #7).
- O teste de PTY (critério #9).
- A perna Windows da matriz de CI **efetivamente verde**, não só configurada.
- Instalação a partir de um artefato publicado numa máquina limpa, sem `cargo`.
