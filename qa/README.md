# QA — Tandem Quality Assurance

## Reports

| Report | What it tests |
|--------|---------------|
| **[REPORT.md](REPORT.md)** | Synthesized findings — start here |
| [naive-agent-report.md](naive-agent-report.md) | Agent with zero docs tries to use tandem via trial-and-error |
| [workflow-eval-report.md](workflow-eval-report.md) | Realistic multi-agent workflow evaluation |
| [stress-report.md](stress-report.md) | Concurrent write correctness under load (5-20 agents) |

## Method

QA was run by **AI subagents** — not shell scripts — because the goal was to
evaluate whether agents can *understand and use* tandem, not just whether
commands return exit code 0.

- **Naive agent:** Given only the binary path. No docs, no source code.
  Documented every attempt, where it got stuck, what error messages helped.
- **Workflow agent:** Given full docs + source. Ran a realistic multi-agent
  collaboration scenario. Evaluated information gaps.
- **Stress agent:** Hammered concurrent writes. Verified CAS correctness
  and persistence across server restarts.

## Key Findings (from initial QA — all issues now resolved)

1. ✅ **Protocol works** — 15/15 integration tests pass, 50 concurrent commits preserved
2. ✅ **Agent discoverability** — `--help`, command suggestions, and `TANDEM_SERVER` env var all implemented
3. ✅ **Code review works** — commits store full file trees, `jj diff`/`jj show` work
4. ✅ **Git push works** — bookmark management available via stock jj commands
5. See `qa/v1/REPORT.md` for the latest usability evaluation
