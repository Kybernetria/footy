import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import { commands, type DesktopIntegrationStatus } from "@/bindings";

/**
 * Live desktop integration status (Flatpak sandbox, portal shortcuts,
 * automatic paste). `null` until the first status arrives.
 */
export function useDesktopIntegration(): DesktopIntegrationStatus | null {
  const [status, setStatus] = useState<DesktopIntegrationStatus | null>(null);

  useEffect(() => {
    let active = true;
    const unlisten = listen<DesktopIntegrationStatus>(
      "desktop-integration-changed",
      (event) => {
        if (active) setStatus(event.payload);
      },
    );
    commands
      .getDesktopIntegrationStatus()
      .then((initial) => {
        if (active) setStatus((current) => current ?? initial);
      })
      .catch((error) =>
        console.warn("Failed to load desktop integration status:", error),
      );
    return () => {
      active = false;
      unlisten.then((fn) => fn());
    };
  }, []);

  return status;
}
