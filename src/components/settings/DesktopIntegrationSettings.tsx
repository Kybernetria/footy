import React, { useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import {
  commands,
  type DesktopIntegrationStatus,
  type PortalState,
} from "@/bindings";
import { SettingsGroup } from "../ui/SettingsGroup";
import { SettingContainer } from "../ui/SettingContainer";
import { ToggleSwitch } from "../ui/ToggleSwitch";
import { Button } from "../ui/Button";

interface DesktopIntegrationSettingsProps {
  status: DesktopIntegrationStatus;
}

const RETRYABLE: PortalState[] = ["denied", "closed", "failed", "inactive"];

const StatusLine: React.FC<{
  tone: "ok" | "muted" | "warn";
  children: React.ReactNode;
}> = ({ tone, children }) => (
  <p
    role="status"
    className={`text-xs ${
      tone === "ok"
        ? "text-text/70"
        : tone === "warn"
          ? "text-amber-500"
          : "text-mid-gray"
    }`}
  >
    {children}
  </p>
);

/**
 * Flatpak on Wayland only: shortcuts and automatic paste go through the
 * desktop's portals, which the user approves and can revoke.
 */
export const DesktopIntegrationSettings: React.FC<
  DesktopIntegrationSettingsProps
> = ({ status }) => {
  const { t } = useTranslation();
  const [pasteBusy, setPasteBusy] = useState(false);
  const [retrying, setRetrying] = useState(false);

  const shortcutState = status.shortcuts_state;
  const shortcutMessage = (() => {
    switch (shortcutState) {
      case "active":
        return t("settings.desktopIntegration.shortcuts.state.active");
      case "connecting":
        return t("settings.desktopIntegration.shortcuts.state.connecting");
      case "denied":
        return t("settings.desktopIntegration.shortcuts.state.denied");
      case "closed":
        return t("settings.desktopIntegration.shortcuts.state.closed");
      case "unsupported":
        return t("settings.desktopIntegration.shortcuts.state.unsupported");
      case "failed":
        return t("settings.desktopIntegration.shortcuts.state.failed", {
          error: status.shortcuts_error ?? "",
        });
      default:
        return t("settings.desktopIntegration.shortcuts.state.inactive");
    }
  })();

  const pasteMessage = (() => {
    if (!status.paste_enabled) {
      return t("settings.desktopIntegration.paste.state.off");
    }
    switch (status.paste_state) {
      case "active":
        return t("settings.desktopIntegration.paste.state.active");
      case "connecting":
        return t("settings.desktopIntegration.paste.state.connecting");
      case "denied":
        return t("settings.desktopIntegration.paste.state.denied");
      case "unsupported":
        return t("settings.desktopIntegration.paste.state.unsupported");
      default:
        return status.paste_error
          ? t("settings.desktopIntegration.paste.state.error", {
              error: status.paste_error,
            })
          : t("settings.desktopIntegration.paste.state.closed");
    }
  })();

  const retryShortcuts = async () => {
    setRetrying(true);
    try {
      const result = await commands.retryPortalShortcuts();
      if (result.status === "error") toast.error(result.error);
    } finally {
      setRetrying(false);
    }
  };

  const setPaste = async (enabled: boolean) => {
    setPasteBusy(true);
    try {
      const result = await commands.setPortalPasteEnabled(enabled);
      if (result.status === "error") toast.error(result.error);
    } finally {
      setPasteBusy(false);
    }
  };

  const copyCommand = async () => {
    try {
      await navigator.clipboard.writeText(status.toggle_command);
      toast.success(t("settings.desktopIntegration.cli.copied"));
    } catch (error) {
      console.error("Failed to copy command:", error);
    }
  };

  const pasteNeedsReconnect =
    status.paste_enabled &&
    RETRYABLE.includes(status.paste_state) &&
    !pasteBusy;

  return (
    <SettingsGroup
      title={t("settings.desktopIntegration.title")}
      description={t("settings.desktopIntegration.description")}
    >
      <SettingContainer
        title={t("settings.desktopIntegration.shortcuts.title")}
        description={t("settings.desktopIntegration.shortcuts.description")}
        descriptionMode="tooltip"
        grouped={true}
        layout="stacked"
      >
        <div className="flex items-center justify-between gap-3">
          <StatusLine
            tone={
              shortcutState === "active"
                ? "ok"
                : shortcutState === "connecting"
                  ? "muted"
                  : "warn"
            }
          >
            {shortcutMessage}
          </StatusLine>
          {RETRYABLE.includes(shortcutState) && (
            <Button
              variant="secondary"
              size="sm"
              onClick={retryShortcuts}
              disabled={retrying}
            >
              {t("settings.desktopIntegration.shortcuts.retry")}
            </Button>
          )}
        </div>
      </SettingContainer>

      <div>
        <ToggleSwitch
          checked={status.paste_enabled}
          onChange={setPaste}
          isUpdating={pasteBusy}
          disabled={status.paste_state === "unsupported"}
          label={t("settings.desktopIntegration.paste.label")}
          description={t("settings.desktopIntegration.paste.description")}
          descriptionMode="tooltip"
          grouped={true}
        />
        <div className="px-4 pb-2 flex items-center justify-between gap-3">
          <StatusLine
            tone={
              !status.paste_enabled
                ? "muted"
                : status.paste_state === "active"
                  ? "ok"
                  : status.paste_state === "connecting"
                    ? "muted"
                    : "warn"
            }
          >
            {pasteMessage}
          </StatusLine>
          {pasteNeedsReconnect && (
            <Button
              variant="secondary"
              size="sm"
              onClick={() => setPaste(true)}
            >
              {t("settings.desktopIntegration.paste.reconnect")}
            </Button>
          )}
        </div>
      </div>

      <SettingContainer
        title={t("settings.desktopIntegration.cli.title")}
        description={t("settings.desktopIntegration.cli.description")}
        descriptionMode="tooltip"
        grouped={true}
        layout="stacked"
      >
        <div className="flex items-center gap-2">
          <code className="flex-1 min-w-0 truncate px-2 py-1 text-xs font-mono bg-mid-gray/10 border border-mid-gray/20 rounded-md select-text">
            {status.toggle_command}
          </code>
          <Button variant="secondary" size="sm" onClick={copyCommand}>
            {t("settings.desktopIntegration.cli.copy")}
          </Button>
        </div>
      </SettingContainer>
    </SettingsGroup>
  );
};
