import type { ReactNode } from "react";
type KernelKind = "task" | "web-app" | "devbox" | "software-factory" | "vm";
export function KindIcon({ kind }: { kind: KernelKind }) {
  let content: ReactNode;
  if (kind === "task") content = <path d="M4 6l1.5 1.5L8.5 4.5M12 6h8M4 12.5l1.5 1.5 3-3M12 12.5h8M4 19l1.5 1.5 3-3M12 19h8" />;
  else if (kind === "web-app") content = <><rect x="3" y="4" width="18" height="16" rx="2" /><path d="M3 8.5h18M8 12.5l-2 2 2 2M16 12.5l2 2-2 2M13.5 11.5l-3 6" /><circle cx="6" cy="6.25" r=".35" /><circle cx="8" cy="6.25" r=".35" /></>;
  else if (kind === "devbox") content = <><rect x="3" y="4" width="18" height="16" rx="2" /><path d="M7 9.5l3 2.5-3 2.5M12.5 15h4" /></>;
  else if (kind === "software-factory") content = <><circle cx="12" cy="12" r="3" /><path d="M12 2.5v2.5M12 19v2.5M2.5 12h2.5M19 12h2.5M5.1 5.1l1.8 1.8M17.1 17.1l1.8 1.8M18.9 5.1l-1.8 1.8M6.9 17.1l-1.8 1.8" /></>;
  else content = <><rect x="3" y="4.5" width="18" height="6" rx="1.5" /><rect x="3" y="13.5" width="18" height="6" rx="1.5" /><path d="M6.5 7.5h.01M6.5 16.5h.01" /></>;
  return <svg className="exe-kind-icon" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">{content}</svg>;
}
