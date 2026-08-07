---
description: Work toward a session goal set with /goal: decompose it, track progress with write_tasks, report against it.
---

# Workflow

1. When a session goal is set, restate it once in your own words — one
   sentence — so a misreading surfaces immediately, not three turns in.
2. Decompose the goal into concrete milestones and record them with
   `write_tasks` (full list, all `pending`). A goal vague enough that you
   cannot list milestones is a goal to clarify with one `ask_user` — three
   concrete readings — before working.
3. Work milestone by milestone, exactly as any other task: mark
   `in_progress` before starting one, verify it, mark `completed`. The task
   list IS the goal's progress meter — keep it truthful; never mark
   completed what you did not verify.
4. At the end of each turn, connect what happened to the goal in one line:
   which milestone advanced, which is next. The user should never have to
   ask "where are we on the goal?"
5. When a request arrives that serves the goal, fold it into the milestones.
   When a request conflicts with or digresses from the goal, do what the
   user asked — their message always outranks the standing goal — but flag
   the tension in one sentence ("done; note this moves us away from <goal>
   because …"). Flag it once, not every turn.

# Guardrails

- The goal is standing context, not a license: it never authorizes actions
  the user didn't ask for in conversation. "The goal implies I should also
  refactor X" is a suggestion to voice, not a change to make.
- Do not let the goal rot: if it is achieved, say so explicitly and suggest
  clearing it; if it has become unreachable or obsolete, say why and ask
  whether to update it. A stale goal silently skews every future turn.
- Scope drift check before each milestone: is this still the shortest path
  to the goal, or momentum from the previous step?

# Definition of done (for the goal itself)

- Every milestone verified `completed`, the goal's outcome demonstrated
  (tests green, artifact produced, question answered — whatever the goal
  named), and a closing summary connecting the work back to the goal's own
  wording.
