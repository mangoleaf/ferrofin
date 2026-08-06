---
name: update-values-example
description: >-
  Keep charts/hermit/values.example.yaml (the shareable, de-identified worked
  example) in sync with the chart schema (charts/hermit/values.yaml) and with
  Hermit's real behavior. Use when the chart's values.yaml or templates change,
  when a new HERMIT_* setting lands, or when asked to "update the example
  values", "sync values.example", "regenerate the values example", "keep the
  chart example current", or "de-identify the values". Run from the repo root
  (/home/mango/dev/hermit).
---

# Keep the chart values-example in sync

`charts/hermit/values.example.yaml` is a **worked, de-identified** example we
ship next to the chart so users can copy-and-adapt a realistic deployment (GPU,
read-only media, split DB/cache volumes). It must stay honest against two sources
of truth and never leak anything identifiable.

## The two sources of truth

1. **The chart schema** — `charts/hermit/values.yaml` (defaults + `# --` docs) and
   `charts/hermit/templates/` (what keys are actually consumed). The example may
   only use keys the chart understands.
2. **Hermit's real behavior** — the app code. Env keys are resolved in
   `apps/hermit-server/src/config.rs` (grep `HERMIT_`); the on-disk layout is
   `crates/hermit-core/src/app_paths.rs` + `crates/hermit-traits/src/system.rs`.
   Claims in comments (e.g. "Hermit exposes /metrics") must match the code.

The **optional** working reference is the real homelab config at
`~/dev/lab/services/hermit/values.yaml` — mine it for realistic patterns, but the
example must be a **de-identified generalization**, never a copy.

## Procedure

1. **Diff schema vs example.** List top-level keys in `charts/hermit/values.yaml`
   and in `values.example.yaml`. For each chart key a real deployment would
   override (image, imagePullSecrets, securityContext, resources, affinity,
   persistence, volumes, volumeMounts, hermit.env/config/envFrom/args, metrics,
   ingress/httpRoute), confirm the example shows it (active or commented). Flag:
   - chart keys missing from the example → add a commented, de-identified example;
   - example keys **not** in the chart schema → drift/typo, fix or remove;
   - renamed keys (chart moved `x`→`y`) → update the example.

2. **Check env keys.** `grep -roE 'HERMIT_[A-Z_]+' apps/hermit-server/src/config.rs`
   and compare to the `hermit.config` block. New user-facing settings (e.g. a new
   `HERMIT_*_DIR`, provider key) should appear as a commented example line.

3. **Verify the load-bearing guidance is intact** (these are why the example
   exists — do not drop them):
   - the cache volume mounts at **`/data/cache`**, not `/data/transcodes`, with the
     rationale that image cache + transcodes both live under `{data_dir}/cache` and
     must stay off the database volume;
   - media volumes are `readOnly: true`;
   - node affinity is called out as REQUIRED for node-local PVs.
   If the code changes where the cache/transcode dirs resolve (see
   `app_paths.rs` `image_cache_path_buf` = `cache_path/images`, and
   `system.rs` `transcode_path` = `cache_path/transcodes`), update the note.

4. **Fix stale claims.** Cross-check comment assertions against code. Known trap:
   the chart's own `values.yaml` once said "Hermit does not expose /metrics yet"
   after `/metrics` shipped — if you spot drift like that in `values.yaml` too,
   flag it (fixing the chart default is in scope when it's a plain factual error).

5. **De-identification gate — MUST pass before finishing.** Run:
   ```
   grep -niE 'mangoleaf|mlstudios|mangoarch|nvme0|/mnt/|k3s|alloy\.loki|gitlab-registry|hermit-cold|hermit-hot|Educational|10\.|192\.168|[a-z0-9.-]+\.svc\.cluster\.local' charts/hermit/values.example.yaml
   ```
   Any hit is a leak. Replace real registries → `registry.example.com/<org>/…`,
   hostnames → `<storage-node>`, claim names → `<...-pvc>`, cluster-internal DNS →
   `<collector>.<namespace>.svc.cluster.local`, IPs/CIDRs → placeholders. Values
   the user must supply use `<angle-bracket>` placeholders.

6. **Validate YAML.**
   ```
   python3 -c "import yaml; list(yaml.safe_load_all(open('charts/hermit/values.example.yaml')))"
   ```

7. **Report** the drift you found and fixed (added/renamed/removed keys, stale
   claims, any de-id leaks caught) so the change is reviewable.

## Guardrails

- The example is documentation, not a deployable file — placeholders are expected
  and correct; do not "resolve" them to real values.
- Keep it copy-ready: a user should be able to swap the `<...>` tokens and deploy.
- Prefer commenting a feature block with a one-line "why" over a bare key dump.
- Never commit real secrets, tokens, IPs, hostnames, or registry URLs.
