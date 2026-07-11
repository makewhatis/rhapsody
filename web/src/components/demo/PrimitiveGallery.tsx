import * as React from "react";
import {
  Button,
  StatusDot,
  StatusChip,
  STATUS_META,
  type StatusKey,
  Pill,
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
} from "@/components/ui";

// PrimitiveGallery — a verification-only route (reached at #/demo, kept out of the app nav)
// that renders every Symphony design-system primitive in every state. It is the manual + smoke
// test surface for the foundation: if a token, variant, or interactive behavior regresses, it
// shows up here. Out of scope for production nav; lazy-loaded by App so it never bloats the
// shipped bundle.

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section style={{ display: "flex", flexDirection: "column", gap: 14 }}>
      <h2
        style={{
          fontSize: 12,
          fontWeight: 600,
          letterSpacing: ".08em",
          textTransform: "uppercase",
          color: "var(--tx-faint)",
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

const SELECT_OPTIONS: SelectOption[] = [
  { value: "claude-opus", label: "Claude Opus", note: "most capable" },
  { value: "claude-sonnet", label: "Claude Sonnet", note: "balanced" },
  { value: "gpt-4o", label: "GPT-4o", note: "fast" },
];

export default function PrimitiveGallery() {
  const [toggleMd, setToggleMd] = React.useState(true);
  const [toggleSm, setToggleSm] = React.useState(false);
  const [check, setCheck] = React.useState(true);
  const [model, setModel] = React.useState("claude-sonnet");
  const [concurrency, setConcurrency] = React.useState(4);
  const [tags, setTags] = React.useState(["infra", "ui"]);
  const [text, setText] = React.useState("");
  const [area, setArea] = React.useState("");

  const statuses = Object.keys(STATUS_META) as StatusKey[];

  return (
    <div style={{ minHeight: "100vh", background: "var(--bg-app)", color: "var(--tx)" }}>
      <div style={{ maxWidth: 1080, margin: "0 auto", padding: "40px 40px 80px", display: "flex", flexDirection: "column", gap: 40 }}>
        <header style={{ display: "flex", flexDirection: "column", gap: 6 }}>
          <h1 style={{ fontSize: 24, fontWeight: 600, letterSpacing: "-0.025em", margin: 0 }}>Symphony primitives</h1>
          <p style={{ fontSize: 13, color: "var(--tx-3)", margin: 0 }}>
            Every design-system primitive in every state — the foundation verification surface (INF-225).
          </p>
        </header>

        <Section title="Buttons">
          <Row>
            <Button variant="primary">Primary</Button>
            <Button variant="subtle">Subtle</Button>
            <Button variant="ghost">Ghost</Button>
            <Button variant="danger">Danger</Button>
            <Button variant="link">Link</Button>
          </Row>
          <Row>
            <Button variant="primary" size="sm">Small</Button>
            <Button variant="subtle" size="md">Medium</Button>
            <Button variant="primary" icon={Plus}>With icon</Button>
            <Button variant="subtle" icon={Wrench} comingSoon>Tools</Button>
          </Row>
          <Row>
            <Button variant="primary" disabled>Disabled</Button>
            <Button variant="subtle" disabled>Disabled</Button>
            <Button variant="danger" disabled>Disabled</Button>
          </Row>
        </Section>

        <Divider />

        <Section title="Status">
          <Row>
            <StatusDot color="var(--em-bright)" pulse />
            <StatusDot color="var(--amber)" />
            <StatusDot color="var(--sky)" />
            <StatusDot color="var(--red)" />
            <StatusDot color="var(--tx-2)" size={6} />
          </Row>
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
        </Section>

        <Section title="Pills">
          <Row>
            <Pill tone="neutral">neutral</Pill>
            <Pill tone="emerald">Healthy</Pill>
            <Pill tone="amber">degraded</Pill>
            <Pill tone="sky">v1.2.0</Pill>
          </Row>
        </Section>

        <Divider />

        <Section title="Cards">
          <Card style={{ width: 280 }}>
            <CardHeader>
              <CardTitle>Card title</CardTitle>
              <CardDescription>A plain shadcn card composed from header + content.</CardDescription>
            </CardHeader>
            <CardContent>
              <div style={{ fontSize: 13, color: "var(--tx-2)" }}>Body content.</div>
            </CardContent>
          </Card>
          <SectionCard
            title="Runtime"
            icon={Cpu}
            desc="Defaults every agent inherits."
            action={<Button variant="subtle" size="sm">Edit</Button>}
            style={{ width: 360 }}
          >
            <div style={{ fontSize: 13, color: "var(--tx-2)" }}>SectionCard with icon, description, and an action slot.</div>
          </SectionCard>
        </Section>

        <Divider />

        <Section title="Fields & inputs">
          <div style={{ width: 360, display: "flex", flexDirection: "column", gap: 16 }}>
            <Field label="Project name" hint="Shown in the runs list.">
              <TextInput value={text} onChange={(e) => setText(e.target.value)} placeholder="symphony-infra" prefixIcon={Folder} />
            </Field>
            <Field label="Search" optional>
              <TextInput defaultValue="" placeholder="Filter…" prefixIcon={Search} suffix="⌘K" />
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
        </Section>

        <Section title="Stepper">
          <Row>
            <Stepper value={concurrency} onChange={setConcurrency} min={1} max={16} suffix="agents" />
          </Row>
        </Section>

        <Section title="Select">
          <Row>
            <Select value={model} options={SELECT_OPTIONS} onChange={setModel} />
            {/* Static invalid+empty example — onChange intentionally inert (state-free demo). */}
            <Select value="" options={SELECT_OPTIONS} onChange={() => {}} placeholder="Pick a model…" invalid />
          </Row>
        </Section>

        <Section title="Toggle & Checkbox">
          <Row>
            <Toggle checked={toggleMd} onChange={setToggleMd} aria-label="Medium toggle" />
            <Toggle checked={toggleSm} onChange={setToggleSm} size="sm" aria-label="Small toggle" />
            <Toggle checked onChange={() => {}} disabled aria-label="Disabled on" />
            <Toggle checked={false} onChange={() => {}} disabled aria-label="Disabled off" />
          </Row>
          <Row>
            <Checkbox checked={check} onChange={setCheck} aria-label="Primary checkbox" />
            <Checkbox checked={false} onChange={() => {}} aria-label="Unchecked" />
            <Checkbox checked onChange={() => {}} disabled aria-label="Disabled checked" />
          </Row>
        </Section>

        <Section title="Chips">
          <div style={{ width: 420 }}>
            <Chips
              items={tags}
              onAdd={(t) => setTags((prev) => [...prev, t])}
              onRemove={(t) => setTags((prev) => prev.filter((x) => x !== t))}
              tone="emerald"
              placeholder="Add a label…"
              invalidItem={(t) => t.length > 16}
            />
          </div>
        </Section>

        <Divider />

        <Section title="Collapsible">
          <div style={{ width: 460, display: "flex", flexDirection: "column", gap: 10 }}>
            <Collapsible label="Advanced settings" icon={Sliders} badge={<Pill tone="neutral">3</Pill>} defaultOpen>
              <div style={{ fontSize: 13, color: "var(--tx-2)" }}>Open by default.</div>
            </Collapsible>
            <Collapsible label="Danger zone" icon={Wrench}>
              <div style={{ fontSize: 13, color: "var(--tx-2)" }}>Collapsed by default.</div>
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

        <Section title="Icons">
          <Row>
            {Object.entries(Icons).map(([name, Icon]) => (
              <span
                key={name}
                title={name}
                style={{ display: "inline-flex", flexDirection: "column", alignItems: "center", gap: 6, width: 64, color: "var(--tx-2)" }}
              >
                <Icon size={18} />
                <span style={{ fontSize: 10, color: "var(--tx-faint)" }}>{name}</span>
              </span>
            ))}
          </Row>
        </Section>
      </div>
    </div>
  );
}
