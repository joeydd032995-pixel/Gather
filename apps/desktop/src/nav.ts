import {
  Combine,
  GitCompareArrows,
  House,
  Images,
  Inbox,
  Layers,
  Library,
  Settings,
  SlidersHorizontal,
  Waypoints,
  type LucideIcon,
} from "lucide-react";

export type Tab =
  | "home"
  | "library"
  | "graph"
  | "review"
  | "clusters"
  | "photos"
  | "contradictions"
  | "entities"
  | "tuning"
  | "settings";

export interface NavItem {
  id: Tab;
  label: string;
  icon: LucideIcon;
  /** Shown in the command palette. */
  hint: string;
}

export interface NavGroup {
  /** Null for the ungrouped top of the sidebar. */
  label: string | null;
  items: NavItem[];
}

export const NAV: NavGroup[] = [
  {
    label: null,
    items: [{ id: "home", label: "Overview", icon: House, hint: "Everything at a glance" }],
  },
  {
    label: "Explore",
    items: [
      { id: "library", label: "Library", icon: Library, hint: "Browse and search everything" },
      {
        id: "graph",
        label: "Graph",
        icon: Waypoints,
        hint: "How people, things and files connect",
      },
      { id: "clusters", label: "Groups", icon: Layers, hint: "Topics and merged duplicates" },
      { id: "photos", label: "Photos", icon: Images, hint: "Albums, duplicates, visual topics" },
    ],
  },
  {
    label: "Needs you",
    items: [
      { id: "review", label: "Review", icon: Inbox, hint: "What the pipeline couldn't decide" },
      {
        id: "contradictions",
        label: "Contradictions",
        icon: GitCompareArrows,
        hint: "Statements that disagree",
      },
      { id: "entities", label: "Entities", icon: Combine, hint: "Possible duplicate entities" },
    ],
  },
  {
    label: "System",
    items: [
      { id: "tuning", label: "Tuning", icon: SlidersHorizontal, hint: "What Gather learned" },
      { id: "settings", label: "Settings", icon: Settings, hint: "Appearance, updates, memory" },
    ],
  },
];

export const NAV_ITEMS: NavItem[] = NAV.flatMap((g) => g.items);
