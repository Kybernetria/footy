import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { commands, type DesktopIntegrationStatus } from "@/bindings";
import { SettingContainer } from "../ui/SettingContainer";
import { Button } from "../ui/Button";
import { useSettings } from "../../hooks/useSettings";

interface PortalShortcutInputProps {
  status: DesktopIntegrationStatus;
  shortcutId: string;
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  disabled?: boolean;
}

/**
 * Flatpak on Wayland: the desktop owns the key combination. Show the trigger
 * it assigned and open its shortcut settings instead of recording keys.
 */
export const PortalShortcutInput: React.FC<PortalShortcutInputProps> = ({
  status,
  shortcutId,
  descriptionMode = "tooltip",
  grouped = false,
  disabled = false,
}) => {
  const { t } = useTranslation();
  const { getSetting } = useSettings();
  const [opening, setOpening] = useState(false);
  const binding = getSetting("bindings")?.[shortcutId];
  const bound = status.shortcuts.find((s) => s.id === shortcutId);
  const active = status.shortcuts_state === "active";
  const canConfigure = active && status.shortcuts_version >= 2;

  const trigger = !active
    ? t("settings.desktopIntegration.shortcuts.notConnected")
    : bound?.trigger
      ? bound.trigger
      : t("settings.desktopIntegration.shortcuts.unassigned");

  const configure = async () => {
    setOpening(true);
    try {
      const result = await commands.configurePortalShortcuts();
      if (result.status === "error") toast.error(result.error);
    } finally {
      setOpening(false);
    }
  };

  return (
    <SettingContainer
      title={t(
        `settings.general.shortcut.bindings.${shortcutId}.name`,
        binding?.name ?? shortcutId,
      )}
      description={t(
        "settings.desktopIntegration.shortcuts.bindingDescription",
      )}
      descriptionMode={descriptionMode}
      grouped={grouped}
      disabled={disabled}
    >
      <div className="flex items-center gap-2">
        <span
          className={`px-2 py-1 text-sm font-semibold rounded-md border ${
            active && bound?.trigger
              ? "bg-mid-gray/10 border-mid-gray/80"
              : "border-mid-gray/30 text-mid-gray"
          }`}
        >
          {trigger}
        </span>
        {canConfigure && (
          <Button
            variant="secondary"
            size="sm"
            onClick={configure}
            disabled={opening || disabled}
          >
            {t("settings.desktopIntegration.shortcuts.change")}
          </Button>
        )}
      </div>
    </SettingContainer>
  );
};
