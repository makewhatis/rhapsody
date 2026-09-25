// The browser layout fixture for the run-detail HEADER (STUDIO-1023).
//
// jsdom does no layout, so the ticket's width acceptance — "at 1440, 1728 and 1920px, and at a
// 400px phone width, nothing overlaps and no value wraps one fragment per line" — can only be
// proven in a real engine. This entry mounts the REAL `TraceHeader` (and the real model functions
// that build its props) inside the console's own `.rh-console > .main > .trrun` ancestors, at a
// viewport the Playwright spec drives. It ships nowhere: `layout.html` is not an entry in the
// production build, and Vite's dev server is the only thing that serves it.
//
// The three fixtures are the ones the acceptance names: a one-attempt ticket, the 11-attempt /
// 25-review STUDIO-988 shape, and a review run.
import { createRef } from "react";
import { createRoot } from "react-dom/client";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import "../index.css";
import "../theme/tokens.css";
import "../theme/console.css";
import "../theme/console-views.css";
import "../theme/console-trace.css";
import type { RunProvenance, RunSummary } from "@/lib/api";
import {
  attemptOptions,
  provenanceFields,
  reviewRounds,
  runVitals,
} from "@/lib/console-trace-view";
import { TraceHeader } from "@/components/console/views/JobDetailView";

const TS = "2026-09-01T19:11:00Z";

function run(over: Partial<RunSummary> & Pick<RunSummary, "id">): RunSummary {
  return {
    issue_id: "i",
    issue_identifier: "STUDIO-988",
    title: "Attach a photo in chat",
    attempt: 0,
    session_uuid: "s",
    branch: "",
    project_slug: "tally",
    repo: "git@github.com:makewhatis/rhapsody.git",
    started_at: TS,
    ended_at: "2026-09-01T19:15:00Z",
    outcome: "completed",
    turns: 3,
    input_tokens: 10,
    output_tokens: 20,
    total_tokens: 38_000,
    usage_estimated: false,
    error: "",
    transcript_path: "",
    ...over,
  };
}

const PROVENANCE: RunProvenance = {
  run_id: 11,
  harness: "claude",
  harness_origin: "profile",
  model: "claude-opus-5-5",
  model_origin: "review.model.claude",
  provider: "anthropic",
  provider_origin: "providers.anthropic",
};

const ROSTER = ["alice", "jimmy", "sol"];

interface Fixture {
  run: RunSummary;
  originTicket: string;
  attempts: ReturnType<typeof attemptOptions>;
  rounds: ReturnType<typeof reviewRounds>;
  who: string;
  assignee: string;
  inFlight: boolean;
  provenance: RunProvenance | undefined;
}

function oneAttempt(): Fixture {
  const runs = [
    run({
      id: 1,
      title: "Make the run-detail header legible at desktop width",
      branch: "symphony/STUDIO-988",
    }),
  ];
  return {
    run: runs[0],
    originTicket: "STUDIO-988",
    attempts: attemptOptions(runs, new Map(), "alice"),
    rounds: [],
    who: "alice",
    assignee: "alice",
    inFlight: false,
    provenance: PROVENANCE,
  };
}

// The shape the ticket measured on STUDIO-988: eleven attempts, twenty-five reviews across five
// pull requests, each with five reviewers.
function manyAttempts(): Fixture {
  const runs = Array.from({ length: 11 }, (_, i) =>
    run({
      id: i + 1,
      title: "Run-detail header: provenance, attempts and reviews",
      branch: "symphony/STUDIO-988",
      started_at: `2026-09-01T19:${String(i).padStart(2, "0")}:00Z`,
    }),
  ).reverse(); // newest-first, exactly as the view receives them
  const reviewers = ["alice", "sol", "jimmy", "bob", "carol"];
  const reviews = Array.from({ length: 25 }, (_, i) => {
    const round = Math.floor(i / 5) + 1;
    const who = reviewers[i % 5];
    return run({
      id: 900 + i,
      issue_identifier: `pr:acme/app#${20 + round}@${who}`,
      started_at: `2026-09-01T${String(round).padStart(2, "0")}:00:00Z`,
    });
  });
  return {
    run: runs[0],
    originTicket: "STUDIO-988",
    attempts: attemptOptions(runs, new Map(), "alice"),
    rounds: reviewRounds(reviews, new Map(), ""),
    who: "alice",
    assignee: "alice",
    inFlight: false,
    provenance: PROVENANCE,
  };
}

// A review run: its key is a `pr:…#223@jimmy` coordinate, its origin ticket is STUDIO-988, and it
// must never be offered Merge.
function reviewRun(): Fixture {
  const review = run({
    id: 801,
    issue_identifier: "pr:makewhatis/rhapsody#223@jimmy",
    branch: "symphony/pr_makewhatis_rhapsody_223_jimmy",
    started_at: "2026-09-01T18:00:00Z",
    ended_at: "",
    outcome: "running",
  });
  return {
    run: review,
    originTicket: "STUDIO-988",
    attempts: [],
    rounds: reviewRounds([review], new Map(), ""),
    who: "jimmy",
    assignee: "",
    inFlight: true,
    provenance: PROVENANCE,
  };
}

const FIXTURES: Record<string, () => Fixture> = {
  one: oneAttempt,
  many: manyAttempts,
  review: reviewRun,
};

// A fetch stub so the few react-query reads the header makes (workspace identity, the daemon's
// Teams flag, the mergeability verdict) answer without a daemon. Nothing here is about the API.
function stubFetch(): void {
  const json = (body: unknown) =>
    Promise.resolve(
      new Response(JSON.stringify(body), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
  window.fetch = (input: RequestInfo | URL) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    if (url.includes("/api/v1/version")) {
      return json({ version: "v0.4.0", commit: "abc", built_at: "", teams_enabled: true });
    }
    if (url.includes("/api/v1/linear/identity")) {
      return json({
        connected: true,
        name: "d",
        display_name: "d",
        email: "d@example.com",
        token: "",
        workspace_url_key: "studio49",
      });
    }
    if (url.includes("/mergeability")) {
      return json({
        mergeable: true,
        receipt: {
          run_id: 801,
          issue: "STUDIO-988",
          pr: "makewhatis/rhapsody#223",
          url: "https://github.com/makewhatis/rhapsody/pull/223",
          number: 223,
          head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
          method: "squash",
          auto: true,
          merge_state: "BLOCKED",
          said: "",
        },
      });
    }
    return json({});
  };
}

const params = new URLSearchParams(window.location.search);
const name = params.get("fixture") ?? "one";
const build = FIXTURES[name] ?? FIXTURES.one;
const fixture = build();
const provenance = fixture.provenance;

const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });

function App() {
  const headerRef = createRef<HTMLDivElement>();
  return (
    <div className="rh-console">
      <div className="main">
        <div className="trrun">
          <TraceHeader
            ref={headerRef}
            run={fixture.run}
            originTicket={fixture.originTicket}
            originPending={false}
            attempts={fixture.attempts}
            rounds={fixture.rounds}
            who={fixture.who}
            resolvingWho={false}
            roster={ROSTER}
            provenance={provenance}
            resolvingProvenance={false}
            vitals={runVitals(fixture.run, [], provenance)}
            inFlight={fixture.inFlight}
            composerId={undefined}
            onBack={() => {}}
            onSelectRun={() => {}}
            onCompose={() => {}}
          />
        </div>
      </div>
    </div>
  );
}

stubFetch();
// Publish the provenance fields the spec asserts on, so the expectations come from the same model
// the header renders rather than a second copy in the test.
declare global {
  interface Window {
    __layoutFixture?: {
      name: string;
      provenance: ReturnType<typeof provenanceFields>;
      branch: string;
    };
  }
}
window.__layoutFixture = {
  name,
  provenance: provenanceFields(provenance),
  branch: runVitals(fixture.run, [], provenance).branch,
};

createRoot(document.getElementById("root")!).render(
  <QueryClientProvider client={queryClient}>
    <App />
  </QueryClientProvider>,
);
