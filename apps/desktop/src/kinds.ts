import {
  File,
  FileImage,
  FileText,
  Image,
  MessagesSquare,
  SquareTerminal,
  type LucideIcon,
} from "lucide-react";

export const KIND_LABELS: Record<string, string> = {
  document_pdf: "PDF",
  document_markdown: "Markdown",
  document_text: "Text",
  image_photo: "Photo",
  image_screenshot: "Screenshot",
  chat_export: "Chat export",
  agent_log: "Agent log",
};

const KIND_ICONS: Record<string, LucideIcon> = {
  document_pdf: FileText,
  document_markdown: FileText,
  document_text: FileText,
  image_photo: Image,
  image_screenshot: FileImage,
  chat_export: MessagesSquare,
  agent_log: SquareTerminal,
};

export function kindLabel(kind: string | null | undefined): string {
  if (!kind) return "Unknown";
  return KIND_LABELS[kind] ?? kind.replace(/_/g, " ");
}

export function kindIcon(kind: string | null | undefined): LucideIcon {
  return (kind && KIND_ICONS[kind]) || File;
}

export function sizeLabel(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** "1 item", "3 items". */
export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n.toLocaleString()} ${n === 1 ? one : many}`;
}
