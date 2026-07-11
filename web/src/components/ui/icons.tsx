import * as React from "react";
import {
  Settings as LSettings,
  Boxes as LBoxes,
  Wrench as LWrench,
  RefreshCw,
  Plus as LPlus,
  Check as LCheck,
  CheckCircle as LCheckCircle,
  ChevronDown as LChevronDown,
  ChevronRight as LChevronRight,
  ChevronLeft as LChevronLeft,
  ArrowLeft as LArrowLeft,
  Search as LSearch,
  X as LX,
  Folder as LFolder,
  Link as LLink,
  Cpu as LCpu,
  Sliders as LSliders,
  HardDrive as LHardDrive,
  Terminal as LTerminal,
  Shield as LShield,
  Trash2,
  AlertTriangle as LAlertTriangle,
  Info as LInfo,
  Activity as LActivity,
  List as LList,
  Eye as LEye,
  Key as LKey,
  Clock as LClock,
  Pause as LPause,
  Code as LCode,
  Download as LDownload,
  ScrollText as LScrollText,
  Play as LPlay,
  Square as LSquare,
  RotateCcw as LRotateCcw,
  type LucideProps,
} from "lucide-react";
import { cn } from "@/lib/utils";

// Symphony icon set — the Claude Design package's `icons.jsx` (~30 lucide-style stroke
// icons) mapped onto `lucide-react`, aliased to the package's names and pre-set to the
// package's 1.6 stroke weight + 16px default size (callers may override either). A couple
// of icons that lucide names differently are aliased (Refresh -> RefreshCw, Trash ->
// Trash2); `Git`, `Linear`, and `Dot` are custom SVGs to match the package exactly (lucide's
// GitBranch is only a two-node glyph, where the design's Git is a three-node fork/merge).

export type IconProps = LucideProps;
export type IconComponent = React.ComponentType<IconProps>;

function wrap(Base: IconComponent, displayName: string): IconComponent {
  const Comp = React.forwardRef<SVGSVGElement, IconProps>(({ className, ...props }, ref) => (
    <Base ref={ref} size={16} strokeWidth={1.6} className={cn("shrink-0", className)} {...props} />
  ));
  Comp.displayName = displayName;
  return Comp;
}

export const Settings = wrap(LSettings, "Settings");
export const Boxes = wrap(LBoxes, "Boxes");
export const Wrench = wrap(LWrench, "Wrench");
export const Refresh = wrap(RefreshCw, "Refresh");
export const Plus = wrap(LPlus, "Plus");
export const Check = wrap(LCheck, "Check");
export const CheckCircle = wrap(LCheckCircle, "CheckCircle");
export const ChevronDown = wrap(LChevronDown, "ChevronDown");
export const ChevronRight = wrap(LChevronRight, "ChevronRight");
export const ChevronLeft = wrap(LChevronLeft, "ChevronLeft");
export const ArrowLeft = wrap(LArrowLeft, "ArrowLeft");
export const Search = wrap(LSearch, "Search");
export const X = wrap(LX, "X");
export const Folder = wrap(LFolder, "Folder");
export const Link = wrap(LLink, "Link");
export const Cpu = wrap(LCpu, "Cpu");
export const Sliders = wrap(LSliders, "Sliders");
export const HardDrive = wrap(LHardDrive, "HardDrive");
export const Terminal = wrap(LTerminal, "Terminal");
export const Shield = wrap(LShield, "Shield");
export const Trash = wrap(Trash2, "Trash");
export const AlertTriangle = wrap(LAlertTriangle, "AlertTriangle");
export const Info = wrap(LInfo, "Info");
export const Activity = wrap(LActivity, "Activity");
export const List = wrap(LList, "List");
export const Eye = wrap(LEye, "Eye");
export const Key = wrap(LKey, "Key");
export const Clock = wrap(LClock, "Clock");
export const Pause = wrap(LPause, "Pause");
export const Code = wrap(LCode, "Code");
export const Download = wrap(LDownload, "Download");
export const ScrollText = wrap(LScrollText, "ScrollText");
export const Play = wrap(LPlay, "Play");
export const Square = wrap(LSquare, "Square");
export const RotateCcw = wrap(LRotateCcw, "RotateCcw");

// Custom: Linear's swoosh logo (not in lucide), ported verbatim from icons.jsx.
export const Linear = React.forwardRef<SVGSVGElement, IconProps>(
  ({ size = 16, strokeWidth = 1.6, className, ...props }, ref) => (
    <svg
      ref={ref}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={strokeWidth}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={cn("shrink-0", className)}
      {...props}
    >
      <path d="M3 13a9 9 0 0 0 8 8M3 8.5A12 12 0 0 1 15.5 21M3.5 17A7 7 0 0 1 7 20.5" />
    </svg>
  ),
);
Linear.displayName = "Linear";

// Custom: the design package's Git glyph — a three-node fork/merge (lucide's GitBranch is
// only two nodes), ported verbatim from icons.jsx.
export const Git = React.forwardRef<SVGSVGElement, IconProps>(
  ({ size = 16, strokeWidth = 1.6, className, ...props }, ref) => (
    <svg
      ref={ref}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={strokeWidth}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={cn("shrink-0", className)}
      {...props}
    >
      <path d="M18 6a3 3 0 1 0 0 6 3 3 0 0 0 0-6ZM6 6a3 3 0 1 0 0 6 3 3 0 0 0 0-6ZM6 18a3 3 0 1 0 0 .01" />
      <path d="M18 9a9 9 0 0 1-9 9M6 12v3" />
    </svg>
  ),
);
Git.displayName = "Git";

// Custom: a filled status dot (the package's Dot is a solid circle, r=5).
export const Dot = React.forwardRef<SVGSVGElement, IconProps>(
  ({ size = 16, className, ...props }, ref) => (
    <svg
      ref={ref}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      className={cn("shrink-0", className)}
      {...props}
    >
      <circle cx={12} cy={12} r={5} fill="currentColor" />
    </svg>
  ),
);
Dot.displayName = "Dot";

// Convenience namespace mirroring the package's `I.<Name>` access pattern.
export const Icons = {
  Settings,
  Boxes,
  Wrench,
  Refresh,
  Plus,
  Check,
  CheckCircle,
  ChevronDown,
  ChevronRight,
  ChevronLeft,
  ArrowLeft,
  Search,
  X,
  Folder,
  Link,
  Git,
  Cpu,
  Sliders,
  HardDrive,
  Terminal,
  Shield,
  Trash,
  AlertTriangle,
  Info,
  Activity,
  List,
  Eye,
  Key,
  Clock,
  Pause,
  Dot,
  Linear,
  Code,
  Download,
} as const;
