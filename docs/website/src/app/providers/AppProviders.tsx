import { ThemeProvider } from "next-themes";
import type { PropsWithChildren } from "react";
import "../../shared/i18n";

export function AppProviders({ children }: PropsWithChildren) {
  return (
    <ThemeProvider
      attribute="data-theme"
      defaultTheme="system"
      enableSystem
      storageKey="xk-theme"
      disableTransitionOnChange
    >
      {children}
    </ThemeProvider>
  );
}
