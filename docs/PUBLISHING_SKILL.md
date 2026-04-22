# Publishing `taskfast-agent` to skills.sh

[skills.sh](https://skills.sh) is a discovery front-end for the
[vercel-labs/skills](https://github.com/vercel-labs/skills) installer
(`npx skills add <owner/repo>`). There is **no submission API, no registry PR,
and no `skills publish` command**. The leaderboard is driven by anonymous
telemetry emitted when users run the installer. "Publishing" this skill means
three things:

1. The skill is discoverable by `npx skills add Akuja-Inc/taskfast-cli` on a
   public GitHub repo (✅ it is — see the validator below).
2. Every release that ships changes to `skills/taskfast-agent/**`
   is validated before merge.
3. The install command is advertised where agent users will see it
   (top-level README, release notes, wiki landing page).

## Pre-release checklist

Run before opening any PR that touches `skills/taskfast-agent/**`:

```bash
make skill-validate       # scripts/validate-skill.sh
```

The validator enforces:

- `skills/taskfast-agent/SKILL.md` exists.
- YAML frontmatter declares `name: taskfast-agent` and a non-empty
  `description`.
- Every `reference/*.md` link in `SKILL.md` resolves to a real file under
  `skills/taskfast-agent/reference/`.
- `npx skills add <repo-path> -l` (the authoritative
  [vercel-labs/skills][vl] installer) surfaces exactly the skill named
  `taskfast-agent`.

To inspect exactly what the installer will hand to a user:

```bash
make skill-preview        # copies the skill into a throwaway tempdir
```

## CI enforcement

`.github/workflows/skill-validate.yml` runs `scripts/validate-skill.sh` on any
PR or push to `main` that touches either the skill tree or the validator. CI
failure blocks merge.

## Release procedure

The skill rides the existing workspace release. No extra step.

1. Land skill changes on `main` with the validator green.
2. `make bump-<patch|minor|major>` — updates the workspace version.
3. Cut the tag; the release workflow (`.github/workflows/release.yml`)
   publishes binaries. The skill itself ships *in-tree* on the tagged commit
   — users installing `Akuja-Inc/taskfast-cli` via `npx skills add` always
   resolve the default branch unless they pin a ref.

## Install command (promotion)

Advertise this snippet in release notes, the README, and the wiki landing
page. Each invocation emits one telemetry ping that contributes to the
leaderboard rank:

```bash
npx skills add Akuja-Inc/taskfast-cli --skill taskfast-agent
```

Optional flags worth documenting for agent users:

- `--global` — install user-wide instead of per-project.
- `--agent claude-code|codex|cursor|...` — scope to a specific client; `*`
  installs into every detected agent directory.
- `--copy` — copy files instead of symlinking (required on Windows or when
  the checkout is on a different filesystem than the agent directory).

## Discovery path

The skill lives at `skills/taskfast-agent/` — a canonical search path for
the `vercel-labs/skills` installer. No recursive-scan fallback involved,
no ambiguity if additional skills land alongside it. New skills for this
repo should follow the same `skills/<name>/SKILL.md` layout.

The evaluation fixture at `evaluation/skill-taskfast-agent/baseline_skill/`
sit outside any canonical path and is not discovered by the installer.

## Troubleshooting

- `skills-validate` reports a missing `reference/*.md` file → either restore
  the file or remove the dead link in `SKILL.md`. The bundler in
  `crates/taskfast-cli/src/cmd/skills.rs` also lists every shipped reference
  file explicitly; keep the two in sync.
- `npx skills add -l` returns zero skills after adding content at a
  canonical path → see "Discovery path — known fragility" above.
- Leaderboard rank not moving after release → the CLI honors
  `DISABLE_TELEMETRY=1`; internal installs with that set do not contribute
  rank. Use a fresh install from a clean environment to smoke-test.

[vl]: https://github.com/vercel-labs/skills
