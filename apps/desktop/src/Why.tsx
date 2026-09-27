// "Why?" — the certificate behind an automatic conclusion, or behind one
// Gather held back, in plain language. Shared by the entity, photo-group,
// contradiction and review views.

import { useEffect, useState } from "react";
import {
  CircleCheck,
  CircleSlash,
  Hand,
  ShieldQuestion,
  Undo2,
  type LucideIcon,
} from "lucide-react";
import {
  getCertificate,
  listCertificates,
  type Certificate,
  type CertificateOutcome,
  type CertificateQuery,
} from "./api";
import { Callout, When, type Tone } from "./ui";

const HEADLINE: Record<string, Record<CertificateOutcome, string>> = {
  entity_merge: {
    auto_applied: "Merged automatically",
    needs_review: "Not merged automatically",
    blocked: "Kept apart",
    user_decision: "Your decision",
    superseded: "An earlier merge was withdrawn",
    retracted: "Merge withdrawn",
  },
  photo_duplicate_group: {
    auto_applied: "Grouped automatically",
    needs_review: "Not grouped automatically",
    blocked: "Kept apart",
    user_decision: "Your decision",
    superseded: "Regrouped",
    retracted: "Group withdrawn",
  },
  contradiction: {
    auto_applied: "Marked contradictory",
    needs_review: "Possible contradiction",
    blocked: "Not marked contradictory",
    user_decision: "Your decision",
    superseded: "Re-evaluated",
    retracted: "Contradiction withdrawn",
  },
  claim_canonicalization: {
    auto_applied: "Confidence increased",
    needs_review: "Needs a look",
    blocked: "Confidence not increased",
    user_decision: "Your decision",
    superseded: "Re-evaluated",
    retracted: "Withdrawn",
  },
};

const DEFAULT_HEADLINE: Record<CertificateOutcome, string> = {
  auto_applied: "Done automatically",
  needs_review: "Waiting for you",
  blocked: "Not done automatically",
  user_decision: "Your decision",
  superseded: "Replaced by newer evidence",
  retracted: "Withdrawn",
};

const LOOK: Record<CertificateOutcome, { tone: Tone; icon: LucideIcon }> = {
  auto_applied: { tone: "success", icon: CircleCheck },
  needs_review: { tone: "warning", icon: ShieldQuestion },
  blocked: { tone: "neutral", icon: CircleSlash },
  user_decision: { tone: "info", icon: Hand },
  superseded: { tone: "neutral", icon: Undo2 },
  retracted: { tone: "neutral", icon: Undo2 },
};

function headline(c: Certificate): string {
  // Canonicalization and corroboration share a kind; only corroboration
  // talks about confidence.
  if (c.conclusion_kind === "claim_canonicalization" && c.rule_id !== "claim.corroboration") {
    return "Same statement, another source";
  }
  return (
    (HEADLINE[c.conclusion_kind] ?? DEFAULT_HEADLINE)[c.outcome] ?? DEFAULT_HEADLINE[c.outcome]
  );
}

/** One certificate, summarized for a person. */
export function CertificateNote({ cert }: { cert: Certificate }) {
  const look = LOOK[cert.outcome] ?? LOOK.blocked;
  const withdrawn = cert.outcome === "superseded" || cert.outcome === "retracted";
  const title = headline(cert);
  // API explanations stand alone ("Not marked contradictory: ..."); under a
  // headline that already says so, drop the repeated lead-in.
  const lead = `${title.toLowerCase()}:`;
  const rest = cert.explanation.slice(lead.length).trim();
  const own = cert.explanation.toLowerCase().startsWith(lead)
    ? rest.charAt(0).toUpperCase() + rest.slice(1)
    : cert.explanation;
  const text =
    withdrawn && cert.status_reason ? `This was withdrawn because ${cert.status_reason}.` : own;
  // Reasons the explanation already says in full aren't repeated.
  const reasons = cert.reasons.filter((r) => !text.includes(r.text));
  return (
    <Callout tone={look.tone} icon={look.icon} title={title}>
      {text && <p>{text}</p>}
      {reasons.length > 0 && !withdrawn && (
        <ul className="why-reasons">
          {reasons.map((r) => (
            <li key={r.code} title={r.code}>
              {r.text}
            </li>
          ))}
        </ul>
      )}
      <details className="why-details">
        <summary>Details</summary>
        <dl className="facts">
          <div>
            <dt>Rule</dt>
            <dd>
              <code>
                {cert.rule_id} v{cert.rule_version}
              </code>
            </dd>
          </div>
          <div>
            <dt>Based on</dt>
            <dd>
              {cert.inputs.length} input{cert.inputs.length === 1 ? "" : "s"} from{" "}
              {cert.source_artifact_ids.length} source file
              {cert.source_artifact_ids.length === 1 ? "" : "s"}
            </dd>
          </div>
          {cert.model_version && (
            <div>
              <dt>Read by</dt>
              <dd>
                <code>{cert.model_version}</code>
              </dd>
            </div>
          )}
          <div>
            <dt>Decided</dt>
            <dd>
              <When iso={cert.created_at} />
            </dd>
          </div>
        </dl>
      </details>
    </Callout>
  );
}

/** Every certificate matching `query` (most recent first), or one by id. */
export default function Why({
  query,
  certificateId,
  limit = 3,
}: {
  query?: CertificateQuery;
  certificateId?: string;
  limit?: number;
}) {
  const [certs, setCerts] = useState<Certificate[] | null>(null);
  const key = certificateId ?? JSON.stringify(query ?? {});
  useEffect(() => {
    let cancelled = false;
    const load = certificateId
      ? getCertificate(certificateId).then((c) => [c])
      : listCertificates({ limit: 20, ...query });
    load
      .then((items) => {
        if (!cancelled) setCerts(items);
      })
      .catch(() => {
        // The explanation is supplementary; the view works without it.
        if (!cancelled) setCerts([]);
      });
    return () => {
      cancelled = true;
    };
  }, [key]);

  if (!certs || certs.length === 0) return null;
  return (
    <section className="why" aria-label="Why">
      {certs.slice(0, limit).map((c) => (
        <CertificateNote key={c.id} cert={c} />
      ))}
    </section>
  );
}
