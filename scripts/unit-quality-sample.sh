#!/usr/bin/env bash
# unit-quality-sample.sh — supports the other half of the Phase 1 "Go" gate
# (docs/TECHNICAL-WRITEUP.md §9): "≥70% of sampled units are judged usable".
#
# "Judged usable" is a human call. Nothing here automates that judgment, and
# nothing here should be read as having done so. What this script provides is
# the part that IS mechanisable: drawing a reproducible random sample with the
# provenance a reviewer needs, and doing the arithmetic afterwards.
#
# Read-only against the database — unlike graph-benchmark.sh, this never writes.
#
#   sample  draw a sample and emit a TSV review sheet with a blank `usable`
#           column for you to fill in with y/n
#   score   read a filled-in sheet back and report the percentage vs the bar
#
# Required environment:
#   DATABASE_URL   the database to sample (safe: reads only)
# Optional environment:
#   GATHER_SAMPLE_SIZE       units to draw   (default 100)
#   GATHER_SAMPLE_SEED       PRNG seed, -1..1 (default 0.42) — echoed into the
#                            sheet so a run can be reproduced exactly
#   GATHER_SAMPLE_THRESHOLD  gate percentage (default 70)
#   GATHER_SAMPLE_MIN_CONF   advisory low-confidence floor (default 0.3)
set -euo pipefail

usage() {
  echo "usage: $0 sample [> sheet.tsv]" >&2
  echo "       $0 score <filled-sheet.tsv>" >&2
  exit 1
}

[ $# -ge 1 ] || usage
mode="$1"

for tool in psql awk; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "unit-quality-sample: required tool '$tool' not found on PATH" >&2
    exit 1
  fi
done

SIZE="${GATHER_SAMPLE_SIZE:-100}"
SEED="${GATHER_SAMPLE_SEED:-0.42}"
THRESHOLD="${GATHER_SAMPLE_THRESHOLD:-70}"
MIN_CONF="${GATHER_SAMPLE_MIN_CONF:-0.3}"

case "$mode" in
sample)
  if [ -z "${DATABASE_URL:-}" ]; then
    echo "unit-quality-sample: DATABASE_URL must be set" >&2
    exit 1
  fi

  # setseed makes the draw reproducible: same seed + same data = same sample,
  # so a disputed result can be re-reviewed rather than re-rolled.
  psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -qtA -F$'\t' <<SQL
SELECT setseed(${SEED});
\\echo # gather unit-quality sample
\\echo # seed=${SEED} size=${SIZE} threshold=${THRESHOLD}%
\\echo #
\\echo # Mark the usable column y or n on every row, then run: score <file>
\\echo #
\\echo # The flag_* columns are ADVISORY ONLY. They detect malformedness, not
\\echo # usefulness, and are deliberately excluded from the score. The gate
\\echo # number comes from the human usable column and nothing else.
\\echo #
SELECT 'usable' AS usable, 'unit_id' AS unit_id, 'kind' AS kind,
       'confidence' AS confidence, 'method' AS method, 'statement' AS statement,
       'subject' AS subject, 'source' AS source, 'quote' AS quote,
       'flag_no_provenance' AS flag_no_provenance, 'flag_no_subject' AS flag_no_subject,
       'flag_dup_statement' AS flag_dup_statement, 'flag_short' AS flag_short,
       'flag_low_conf' AS flag_low_conf;
SELECT ''                                              AS usable,
       u.id::text                                      AS unit_id,
       u.kind::text                                    AS kind,
       round(u.confidence::numeric, 2)::text           AS confidence,
       u.extraction_method::text                       AS method,
       replace(replace(u.statement, E'\t', ' '), E'\n', ' ')  AS statement,
       coalesce(e.name, '')                            AS subject,
       coalesce(a.source_platform || '/' || a.kind::text, '') AS source,
       coalesce(replace(replace(left(p.quote, 160), E'\t', ' '), E'\n', ' '), '') AS quote,
       (p.id IS NULL)::text                            AS flag_no_provenance,
       (u.subject_entity_id IS NULL)::text             AS flag_no_subject,
       (dup.n > 1)::text                               AS flag_dup_statement,
       (length(u.statement) < 12)::text                AS flag_short,
       (u.confidence < ${MIN_CONF})::text              AS flag_low_conf
FROM (SELECT * FROM atomic_units ORDER BY random() LIMIT ${SIZE}) u
LEFT JOIN LATERAL (
  SELECT pr.id, pr.quote, pr.artifact_id
  FROM atomic_unit_provenance pr
  WHERE pr.atomic_unit_id = u.id
  ORDER BY pr.id LIMIT 1
) p ON true
LEFT JOIN artifacts a ON a.id = p.artifact_id
LEFT JOIN entities  e ON e.id = u.subject_entity_id
LEFT JOIN LATERAL (
  SELECT count(*) AS n FROM atomic_units d WHERE d.statement_hash = u.statement_hash
) dup ON true
ORDER BY u.id;
SQL
  ;;

score)
  [ $# -eq 2 ] || usage
  sheet="$2"
  [ -f "$sheet" ] || { echo "unit-quality-sample: no such file: $sheet" >&2; exit 1; }

  awk -F'\t' -v threshold="$THRESHOLD" '
    /^#/      { next }                       # header comments
    $1=="usable" { next }                    # column header row
    NF < 2    { next }                       # blank/short lines
    {
      total++
      v = tolower($1)
      gsub(/^[ \t]+|[ \t]+$/, "", v)
      if (v == "y" || v == "yes" || v == "1")      { usable++ }
      else if (v == "n" || v == "no" || v == "0")  { unusable++ }
      else                                          { unmarked++ }
    }
    END {
      if (total == 0) {
        print "unit-quality-sample: sheet contains no sample rows" > "/dev/stderr"
        exit 1
      }
      # An unmarked row is not a "no" — scoring a partially reviewed sheet
      # would silently understate the result. Refuse instead.
      if (unmarked > 0) {
        printf "unit-quality-sample: %d of %d rows are unmarked; mark every row y/n first\n", unmarked, total > "/dev/stderr"
        exit 1
      }
      pct = 100.0 * usable / total
      printf "sample size:  %d\n", total
      printf "judged usable: %d (%.1f%%)\n", usable, pct
      printf "judged not:    %d\n", unusable
      printf "gate:          >=%s%% — %s\n", threshold, (pct >= threshold ? "PASS" : "FAIL")
      exit (pct >= threshold ? 0 : 1)
    }
  ' "$sheet"
  ;;

*)
  usage
  ;;
esac
