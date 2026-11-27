import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { commands } from "@/bindings";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { useSettings } from "../../hooks/useSettings";

interface AutostartToggleProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  /** Flatpak: launch at login goes through the desktop's Background portal. */
  sandboxed?: boolean;
  supported?: boolean;
}

export const AutostartToggle: React.FC<AutostartToggleProps> = React.memo(
  ({
    descriptionMode = "tooltip",
    grouped = false,
    sandboxed = false,
    supported = true,
  }) => {
    const { t } = useTranslation();
    const { getSetting, updateSetting, isUpdating, refreshSettings } =
      useSettings();
    const [requesting, setRequesting] = useState(false);

    const autostartEnabled = getSetting("autostart_enabled") ?? false;

    const onChange = async (enabled: boolean) => {
      if (!sandboxed) {
        updateSetting("autostart_enabled", enabled);
        return;
      }
      // The desktop may decline; show what it actually granted.
      setRequesting(true);
      try {
        const result = await commands.changeAutostartSetting(enabled);
        if (result.status === "error") toast.error(result.error);
      } finally {
        await refreshSettings();
        setRequesting(false);
      }
    };

    return (
      <ToggleSwitch
        checked={autostartEnabled && supported}
        onChange={onChange}
        disabled={!supported}
        isUpdating={isUpdating("autostart_enabled") || requesting}
        label={t("settings.advanced.autostart.label")}
        description={
          supported
            ? t("settings.advanced.autostart.description")
            : t("settings.advanced.autostart.unavailable")
        }
        descriptionMode={descriptionMode}
        grouped={grouped}
      />
    );
  },
);
