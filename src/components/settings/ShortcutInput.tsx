import React from "react";
import { useSettings } from "../../hooks/useSettings";
import { GlobalShortcutInput } from "./GlobalShortcutInput";
import { HandyKeysShortcutInput } from "./HandyKeysShortcutInput";
import { PortalShortcutInput } from "./PortalShortcutInput";
import { useDesktopIntegration } from "../../hooks/useDesktopIntegration";

interface ShortcutInputProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  shortcutId: string;
  disabled?: boolean;
}

/**
 * Wrapper component that selects the appropriate shortcut input implementation
 * based on the keyboard_implementation setting.
 *
 * - "tauri" (default): Uses GlobalShortcutInput with JS keyboard events
 * - "handy_keys": Uses HandyKeysShortcutInput with backend key events
 * - Flatpak on Wayland: PortalShortcutInput shows the desktop's trigger
 */
export const ShortcutInput: React.FC<ShortcutInputProps> = (props) => {
  const { getSetting } = useSettings();
  const integration = useDesktopIntegration();
  const keyboardImplementation = getSetting("keyboard_implementation");

  // Flatpak on Wayland: the desktop owns the shortcut through its portal.
  if (integration?.portal_mode) {
    return <PortalShortcutInput status={integration} {...props} />;
  }

  // Default to Tauri implementation if not set
  if (keyboardImplementation === "handy_keys") {
    return <HandyKeysShortcutInput {...props} />;
  }

  return <GlobalShortcutInput {...props} />;
};
