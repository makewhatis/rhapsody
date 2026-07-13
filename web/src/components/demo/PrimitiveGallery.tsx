import * as React from "react";
import {
  Button,
  StatusChip,
  STATUS_META,
  type StatusKey,
  Pill,
  StatusDot,
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
  SectionCard,
  Divider,
  Skeleton,
  SkeletonCard,
  Field,
  FieldError,
  TextInput,
  TextArea,
  Stepper,
  Select,
  type SelectOption,
  Toggle,
  Checkbox,
  Chips,
  Collapsible,
  Icons,
  Cpu,
  Sliders,
  Folder,
  Search,
  Plus,
  Wrench,
  Play,
  Square,
  RotateCcw,
} from "@/components/ui";

// PrimitiveGallery — a verification-only route (reached at #/demo, kept out of the app nav)
// that renders every design-system primitive in every state, re-tokened for the "Podium"
// reskin per mock 2f. It is the manual + smoke test surface for the foundation: if a token,
// variant, or interactive behavior regresses, it shows up here. Out of scope for production
// nav; lazy-loaded by App so it never bloats the shipped bundle.

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section style={{ display: "flex", flexDirection: "column", gap: 16 }}>
      <h2
        style={{
          fontSize: 10,
          fontWeight: 600,
          letterSpacing: ".12em",
          textTransform: "uppercase",
          color: "var(--faint)",
          margin: 0,
        }}
      >
        {title}
      </h2>
      <div style={{ display: "flex", flexWrap: "wrap", gap: 16, alignItems: "flex-start" }}>{children}</div>
    </section>
  );
}

const Row = ({ children }: { children: React.ReactNode }) => (
  <div style={{ display: "flex", flexWrap: "wrap", gap: 12, alignItems: "center" }}>{children}</div>
);

// ---- Tokens: color swatches (the Design Tokens table) ----
const SWATCHES: { name: string; hex: string; token: string }[] = [
  { name: "Ground", hex: "#141210", token: "--ground" },
  { name: "Surface", hex: "#1C1916", token: "--surface" },
  { name: "Card", hex: "#1A1714", token: "--card" },
  { name: "Ink", hex: "#EDE7E1", token: "--ink" },
  { name: "Muted", hex: "#A59C90", token: "--text-muted" },
  { name: "Faint", hex: "#6E675E", token: "--faint" },
  { name: "Rust", hex: "#C25B2E", token: "--rust" },
  { name: "Rust text", hex: "#E08653", token: "--rust-text" },
  { name: "Sage", hex: "#97AE87", token: "--sage" },
  { name: "Amber", hex: "#CDA35A", token: "--amber" },
  { name: "Red", hex: "#E0574C", token: "--red" },
  { name: "Slate", hex: "#86A9C6", token: "--slate" },
];

function Swatch({ name, hex, token }: { name: string; hex: string; token: string }) {
  return (
    <div style={{ display: "flex", alignItems: "center", gap: 10, width: 168 }}>
      <span
        style={{
          width: 30,
          height: 30,
          borderRadius: 8,
          background: `var(${token})`,
          border: "1px solid var(--hair-strong)",
          flexShrink: 0,
        }}
      />
      <span style={{ display: "flex", flexDirection: "column", gap: 1, minWidth: 0 }}>
        <span style={{ fontSize: 12, fontWeight: 500, color: "var(--ink)" }}>{name}</span>
        <span className="mono" style={{ fontSize: 10.5, color: "var(--faint)" }}>
          {hex}
        </span>
      </span>
    </div>
  );
}

// ---- Type ramp ----
function TypeSample({ children, note }: { children: React.ReactNode; note: string }) {
  return (
    <div style={{ display: "flex", flexDirection: "column", gap: 4 }}>
      <div>{children}</div>
      <div className="mono" style={{ fontSize: 10.5, color: "var(--faint)" }}>
        {note}
      </div>
    </div>
  );
}

// ---- inline demo: segmented control ----
function Segmented({ items }: { items: string[] }) {
  const [active, setActive] = React.useState(0);
  return (
    <div
      style={{
        display: "inline-flex",
        gap: 2,
        padding: 2,
        background: "rgba(255,255,255,.04)",
        border: "1px solid var(--hair-section)",
        borderRadius: 7,
      }}
    >
      {items.map((it, i) => (
        <button
          key={it}
          type="button"
          onClick={() => setActive(i)}
          style={{
            border: "none",
            cursor: "pointer",
            fontSize: 11.5,
            padding: "3px 9px",
            borderRadius: 5,
            background: i === active ? "rgba(255,255,255,.09)" : "transparent",
            color: i === active ? "var(--ink)" : "var(--text-muted)",
          }}
        >
          {it}
        </button>
      ))}
    </div>
  );
}

// ---- inline demo: radio group ----
function RadioRow() {
  const [sel, setSel] = React.useState(0);
  return (
    <div style={{ display: "inline-flex", gap: 14 }}>
      {[0, 1].map((i) => (
        <button
          key={i}
          type="button"
          role="radio"
          aria-checked={sel === i}
          aria-label={`Option ${i + 1}`}
          onClick={() => setSel(i)}
          style={{
            width: 18,
            height: 18,
            borderRadius: "50%",
            border: `1.5px solid ${sel === i ? "var(--rust)" : "var(--hair-strong)"}`,
            background: "transparent",
            display: "grid",
            placeItems: "center",
            cursor: "pointer",
            padding: 0,
          }}
        >
          {sel === i ? (
            <span style={{ width: 8, height: 8, borderRadius: "50%", background: "var(--rust)" }} />
          ) : null}
        </button>
      ))}
    </div>
  );
}

// ---- inline demo: conductor status cluster (toolbar) ----
function ConductorStatus() {
  return (
    <div
      style={{
        display: "inline-flex",
        alignItems: "center",
        gap: 9,
        padding: "9px 14px",
        background: "var(--surface)",
        border: "1px solid var(--hair-card)",
        borderRadius: 8,
      }}
    >
      <span style={{ fontSize: 13, fontWeight: 600, color: "var(--ink)", letterSpacing: ".01em" }}>Rhapsody</span>
      <StatusDot color="var(--rust-text)" pulse size={6} />
      <span style={{ fontSize: 12, color: "var(--text-muted)" }}>Playing — 1 agent</span>
      <span className="mono" style={{ fontSize: 10.5, color: "var(--faint)" }}>
        daemon healthy · poll 2s
      </span>
    </div>
  );
}

// ---- inline demo: transport segment (play / stop / restart) ----
function TransportSegment() {
  const cells: { icon: React.ReactNode; label: string; enabled: boolean }[] = [
    { icon: <Play size={13} />, label: "Play", enabled: false },
    { icon: <Square size={11} />, label: "Stop", enabled: true },
    { icon: <RotateCcw size={13} />, label: "Restart", enabled: true },
  ];
  return (
    <div
      style={{
        display: "inline-flex",
        border: "1px solid var(--hair-strong)",
        borderRadius: 7,
        overflow: "hidden",
      }}
    >
      {cells.map((c, i) => (
        <span
          key={c.label}
          title={c.label}
          style={{
            width: 34,
            height: 28,
            display: "grid",
            placeItems: "center",
            background: c.enabled ? "rgba(255,255,255,.04)" : "rgba(255,255,255,.02)",
            color: c.enabled ? "var(--text-2)" : "var(--faint)",
            borderLeft: i === 0 ? "none" : "1px solid rgba(255,255,255,.08)",
          }}
        >
          {c.icon}
        </span>
      ))}
    </div>
  );
}

// ---- inline demo: stat cell ----
function StatCell() {
  return (
    <div
      style={{
        display: "flex",
        flexDirection: "column",
        gap: 5,
        padding: "12px 18px",
        border: "1px solid var(--hair-card)",
        borderRadius: 10,
        minWidth: 170,
      }}
    >
      <span style={{ fontSize: 10, fontWeight: 600, letterSpacing: ".12em", textTransform: "uppercase", color: "var(--faint)" }}>
        Stat cell
      </span>
      <div style={{ display: "flex", alignItems: "baseline", gap: 8 }}>
        <span style={{ fontSize: 22, fontWeight: 600, color: "var(--ink)", fontVariantNumeric: "tabular-nums" }}>4.9M</span>
        <span className="mono" style={{ fontSize: 11, color: "var(--faint)" }}>
          105 in · 60.5k out
        </span>
      </div>
    </div>
  );
}

// ---- inline demo: rhythm sparkline (opacity-ramped rust bars) ----
function Sparkline() {
  const heights = [6, 9, 5, 12, 8, 14, 10, 16];
  return (
    <span style={{ display: "inline-flex", alignItems: "flex-end", gap: 2, height: 16 }}>
      {heights.map((h, i) => (
        <span
          key={i}
          style={{
            width: 2,
            height: h,
            background: i === heights.length - 1 ? "var(--rust-text)" : "var(--rust)",
            opacity: i === heights.length - 1 ? 1 : 0.3 + (i / heights.length) * 0.55,
          }}
        />
      ))}
    </span>
  );
}

// ---- inline demo: live-row rule ----
function LiveRowRule() {
  return (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 10 }}>
      <span style={{ width: 2, height: 18, background: "var(--rust)", borderRadius: 1 }} />
      <span style={{ fontSize: 12, color: "var(--rust-text)" }}>live row rule</span>
      <span className="mono" style={{ fontSize: 11.5, color: "var(--rust-text)" }}>
        14m 21s
      </span>
    </span>
  );
}

// ---- inline demo: tool-call chip ----
function ToolCallChip() {
  return (
    <span style={{ display: "inline-flex", alignItems: "center", gap: 10 }}>
      <span
        className="mono"
        style={{
          fontSize: 10.5,
          fontWeight: 600,
          color: "var(--rust-text)",
          background: "var(--tint-rust)",
          border: "1px solid color-mix(in srgb, var(--rust-text) 35%, transparent)",
          borderRadius: 5,
          padding: "2px 8px",
        }}
      >
        → Bash
      </span>
      <span style={{ fontSize: 12, color: "var(--text-muted)" }}>tool call chip</span>
    </span>
  );
}

const SELECT_OPTIONS: SelectOption[] = [
  { value: "opus-4-8", label: "Claude Opus", note: "most capable" },
  { value: "sonnet", label: "Claude Sonnet", note: "balanced" },
  { value: "gpt-4o", label: "GPT-4o", note: "fast" },
];

export default function PrimitiveGallery() {
  const [toggleMd, setToggleMd] = React.useState(true);
  const [toggleSm, setToggleSm] = React.useState(false);
  const [check, setCheck] = React.useState(true);
  const [model, setModel] = React.useState("sonnet");
  const [concurrency, setConcurrency] = React.useState(20);
  const [tags, setTags] = React.useState(["infra", "ui"]);
  const [text, setText] = React.useState("");
  const [area, setArea] = React.useState("");

  const statuses = Object.keys(STATUS_META) as StatusKey[];

  return (
    <div style={{ minHeight: "100vh", background: "var(--ground)", color: "var(--ink)" }}>
      <div style={{ maxWidth: 1080, margin: "0 auto", padding: "40px 40px 80px", display: "flex", flexDirection: "column", gap: 40 }}>
        <header style={{ display: "flex", flexDirection: "column", gap: 6 }}>
          <h1 style={{ fontSize: 22, fontWeight: 600, letterSpacing: "-0.02em", margin: 0 }}>Rhapsody primitives</h1>
          <p style={{ fontSize: 12.5, color: "var(--text-muted)", margin: 0 }}>
            Every design-system primitive in every state — the foundation verification surface (INF-225), re-tokened for
            Podium.
          </p>
        </header>

        <Section title="Tokens">
          {SWATCHES.map((s) => (
            <Swatch key={s.token} {...s} />
          ))}
        </Section>

        <Section title="Type">
          <TypeSample note="SF Pro 17/600">
            <span style={{ fontSize: 17, fontWeight: 600, color: "var(--ink)" }}>Page title</span>
          </TypeSample>
          <TypeSample note="13/600">
            <span style={{ fontSize: 13, fontWeight: 600, color: "var(--ink)" }}>Card title</span>
          </TypeSample>
          <TypeSample note="12.5/400 muted">
            <span style={{ fontSize: 12.5, color: "var(--text-muted)" }}>Body / description text</span>
          </TypeSample>
          <TypeSample note="10/600 · .12em">
            <span style={{ fontSize: 10, fontWeight: 600, letterSpacing: ".12em", textTransform: "uppercase", color: "var(--faint)" }}>
              Caps label
            </span>
          </TypeSample>
          <TypeSample note="SF Mono 11.5 tabular">
            <span className="mono" style={{ fontSize: 11.5, color: "var(--text-muted)" }}>
              mono data 132.4k · 14m 21s
            </span>
          </TypeSample>
          <TypeSample note="stat numeral 22/600">
            <span style={{ fontSize: 22, fontWeight: 600, color: "var(--ink)", fontVariantNumeric: "tabular-nums" }}>4.9M</span>
          </TypeSample>
        </Section>

        <Divider />

        <Section title="Buttons">
          <Row>
            <Button variant="primary">Primary</Button>
            <Button variant="subtle">Subtle</Button>
            <Button variant="ghost">Ghost</Button>
            <Button variant="danger">Danger</Button>
            <Button variant="link">Link</Button>
            <Button variant="primary" size="sm">
              Small
            </Button>
            <Button variant="primary" icon={Plus}>
              With icon
            </Button>
          </Row>
          <Row>
            <Button variant="primary" disabled>
              Disabled
            </Button>
            <Button variant="subtle" disabled>
              Disabled
            </Button>
            <Button variant="danger" disabled>
              Disabled
            </Button>
          </Row>
        </Section>

        <Divider />

        <Section title="Status">
          <Row>
            {statuses.map((s) => (
              <StatusChip key={s} status={s} />
            ))}
          </Row>
          <Row>
            <StatusChip status="running" count={3} />
            <StatusChip status="queued" count={12} />
            <StatusChip status="failed" count={1} />
          </Row>
          <Row>
            <Pill tone="neutral">neutral</Pill>
            <Pill tone="sage" dot>
              healthy
            </Pill>
            <Pill tone="amber" dot>
              degraded
            </Pill>
            <Pill tone="slate">v0.1.0</Pill>
          </Row>
        </Section>

        <Divider />

        <Section title="Controls">
          <div style={{ width: 260 }}>
            <TextInput defaultValue="" placeholder="Search jobs…" prefixIcon={Search} suffix="⌘K" />
          </div>
          <Toggle checked={toggleMd} onChange={setToggleMd} aria-label="Medium toggle" />
          <Toggle checked={toggleSm} onChange={setToggleSm} size="sm" aria-label="Small toggle" />
          <Checkbox checked={check} onChange={setCheck} aria-label="Primary checkbox" />
          <Stepper value={concurrency} onChange={setConcurrency} min={1} max={99} style={{ width: 150 }} />
          <Select value={model} options={SELECT_OPTIONS} onChange={setModel} width={200} />
          <Segmented items={["All", "Warn+", "Error"]} />
          <RadioRow />
        </Section>

        <Divider />

        <Section title="Fields & inputs">
          <div style={{ width: 360, display: "flex", flexDirection: "column", gap: 16 }}>
            <Field label="Project name" hint="Shown in the runs list.">
              <TextInput value={text} onChange={(e) => setText(e.target.value)} placeholder="rhapsody-infra" prefixIcon={Folder} />
            </Field>
            <Field label="Token" error="A token is required.">
              <TextInput defaultValue="" invalid mono placeholder="lin_api_…" />
            </Field>
            <Field label="Disabled">
              <TextInput defaultValue="read-only" disabled />
            </Field>
            <Field label="Notes" hint="Markdown supported.">
              <TextArea value={area} onChange={(e) => setArea(e.target.value)} rows={3} placeholder="Anything worth noting…" />
            </Field>
            <FieldError>Standalone validation message.</FieldError>
          </div>
          <div style={{ width: 380 }}>
            <Chips
              items={tags}
              onAdd={(t) => setTags((prev) => [...prev, t])}
              onRemove={(t) => setTags((prev) => prev.filter((x) => x !== t))}
              tone="sage"
              placeholder="Add a label…"
              invalidItem={(t) => t.length > 16}
            />
          </div>
        </Section>

        <Section title="Collapsible">
          <div style={{ width: 460, display: "flex", flexDirection: "column", gap: 10 }}>
            <Collapsible label="Advanced settings" icon={Sliders} badge={<Pill tone="neutral">3</Pill>} defaultOpen>
              <div style={{ fontSize: 13, color: "var(--text-muted)" }}>Open by default.</div>
            </Collapsible>
            <Collapsible label="Danger zone" icon={Wrench}>
              <div style={{ fontSize: 13, color: "var(--text-muted)" }}>Collapsed by default.</div>
            </Collapsible>
          </div>
        </Section>

        <Section title="Skeletons">
          <div style={{ display: "flex", flexDirection: "column", gap: 10, width: 320 }}>
            <Skeleton w={220} h={16} />
            <Skeleton w="100%" h={12} />
            <Skeleton w={120} h={12} />
          </div>
          <div style={{ width: 320 }}>
            <SkeletonCard />
          </div>
        </Section>

        <Divider />

        <Section title="New clusters">
          <ConductorStatus />
          <TransportSegment />
          <StatCell />
          <Sparkline />
          <LiveRowRule />
          <ToolCallChip />
        </Section>

        <Divider />

        <Section title="Cards">
          <Card style={{ width: 280 }}>
            <CardHeader>
              <CardTitle>Card title</CardTitle>
              <CardDescription>A plain card composed from header + content.</CardDescription>
            </CardHeader>
            <CardContent>
              <div style={{ fontSize: 13, color: "var(--text-muted)" }}>Body content.</div>
            </CardContent>
          </Card>
          <SectionCard
            title="Runtime"
            icon={Cpu}
            desc="Defaults every agent inherits."
            action={
              <Button variant="subtle" size="sm">
                Edit
              </Button>
            }
            style={{ width: 360 }}
          >
            <div style={{ fontSize: 13, color: "var(--text-muted)" }}>SectionCard with icon, description, and an action slot.</div>
          </SectionCard>
        </Section>

        <Divider />

        <Section title="Icons">
          <Row>
            {Object.entries(Icons).map(([name, Icon]) => (
              <span
                key={name}
                title={name}
                style={{ display: "inline-flex", flexDirection: "column", alignItems: "center", gap: 6, width: 64, color: "var(--text-muted)" }}
              >
                <Icon size={18} />
                <span style={{ fontSize: 10, color: "var(--faint)" }}>{name}</span>
              </span>
            ))}
          </Row>
        </Section>
      </div>
    </div>
  );
}
