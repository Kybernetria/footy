import React, { useEffect, useState, useRef } from "react";
import { useTranslation } from "react-i18next";
import {
  getKeyName,
  formatKeyCombination,
  normalizeKey,
} from "../../lib/utils/keyboard";
import { ResetButton } from "../ui/ResetButton";
import { SettingContainer } from "../ui/SettingContainer";
import { useSettings } from "../../hooks/useSettings";
import { useOsType } from "../../hooks/useOsType";
import { commands } from "@/bindings";
import { toast } from "sonner";
import {
  runGlobalShortcutBindingOperation,
  type BindingOperationSummary,
} from "./globalShortcutLifecycle";

interface GlobalShortcutInputProps {
  descriptionMode?: "inline" | "tooltip";
  grouped?: boolean;
  shortcutId: string;
  disabled?: boolean;
}

interface RecordingSession {
  id: string;
  originalBinding: string;
  bindingIds: string[];
  suspendPromise: Promise<BindingOperationSummary>;
  finalizePromise?: Promise<void>;
}

export const GlobalShortcutInput: React.FC<GlobalShortcutInputProps> = ({
  descriptionMode = "tooltip",
  grouped = false,
  shortcutId,
  disabled = false,
}) => {
  const { t } = useTranslation();
  const { getSetting, updateBinding, resetBinding, isUpdating, isLoading } =
    useSettings();
  const [keyPressed, setKeyPressed] = useState<string[]>([]);
  const [recordedKeys, setRecordedKeys] = useState<string[]>([]);
  const [editingShortcutId, setEditingShortcutId] = useState<string | null>(
    null,
  );
  const shortcutRefs = useRef<Map<string, HTMLDivElement | null>>(new Map());
  const osType = useOsType();

  const bindings = getSetting("bindings") || {};
  const postProcessEnabled = getSetting("post_process_enabled") ?? false;
  const relevantBindingIds = Object.keys(bindings).filter(
    (id) =>
      id !== "cancel" &&
      (id !== "transcribe_with_post_process" || postProcessEnabled),
  );
  const mountedRef = useRef(true);
  const recordingSessionRef = useRef<RecordingSession | null>(null);
  const updateBindingRef = useRef(updateBinding);
  const translationRef = useRef(t);
  updateBindingRef.current = updateBinding;
  translationRef.current = t;

  const suspendBindings = (ids: string[]): Promise<BindingOperationSummary> =>
    runGlobalShortcutBindingOperation(ids, (id) => commands.suspendBinding(id));

  const resumeBindings = (ids: string[]): Promise<BindingOperationSummary> =>
    runGlobalShortcutBindingOperation(ids, (id) => commands.resumeBinding(id));

  const reportBindingErrors = (
    action: "suspend" | "resume",
    errors: BindingOperationSummary["errors"],
  ) => {
    errors.forEach(({ id, error }) => {
      const message = `Failed to ${action} shortcut '${id}': ${String(error)}`;
      console.error(message);
      toast.error(message);
    });
  };

  const finishRecording = (
    session: RecordingSession,
    newBinding?: string,
  ): Promise<void> => {
    // A keyup, click, and unmount can race each other. The first finalizer
    // owns the session; all later callers wait for the same cleanup.
    if (session.finalizePromise) return session.finalizePromise;

    session.finalizePromise = (async () => {
      const suspendResult = await session.suspendPromise;
      reportBindingErrors("suspend", suspendResult.errors);
      // Restore other shortcuts before changing this one, so backend duplicate
      // detection still prevents stealing another action's key combination.
      const otherResumeResult = await resumeBindings(
        suspendResult.successfulIds.filter((id) => id !== session.id),
      );
      reportBindingErrors("resume", otherResumeResult.errors);
      const canUpdate =
        suspendResult.errors.length === 0 &&
        otherResumeResult.errors.length === 0;
      let bindingUpdated = false;
      let bindingRegistered = false;

      if (canUpdate && newBinding !== undefined) {
        try {
          await updateBindingRef.current(session.id, newBinding);
          bindingUpdated = true;
          bindingRegistered = true;
        } catch (error) {
          console.error("Failed to change binding:", error);
          toast.error(
            translationRef.current("settings.general.shortcut.errors.set", {
              error: String(error),
            }),
          );
        }
      }

      // Only a failed update needs the original value restored. Cancellation
      // before an update does not change the binding.
      if (
        canUpdate &&
        newBinding !== undefined &&
        !bindingUpdated &&
        session.originalBinding
      ) {
        try {
          await updateBindingRef.current(session.id, session.originalBinding);
          bindingRegistered = true;
        } catch (error) {
          console.error("Failed to restore original binding:", error);
          toast.error(
            translationRef.current("settings.general.shortcut.errors.reset"),
          );
        }
      }

      // changeBinding registers the edited shortcut itself. Registering it
      // again can fail as a duplicate on the Tauri shortcut backend.
      const resumeResult = await resumeBindings([
        ...suspendResult.successfulIds.filter(
          (id) => id === session.id && !bindingRegistered,
        ),
        // A transient backend failure must not strand another action after
        // aborting the edit. Retry just the failed registrations once.
        ...otherResumeResult.errors.map(({ id }) => id),
      ]);
      reportBindingErrors("resume", resumeResult.errors);
      if (resumeResult.errors.length > 0) {
        toast.error(
          "Some shortcuts could not be restored. Restart Footy to restore them.",
        );
      }

      if (recordingSessionRef.current === session) {
        recordingSessionRef.current = null;
        if (mountedRef.current) {
          setEditingShortcutId(null);
          setKeyPressed([]);
          setRecordedKeys([]);
        }
      }
    })();

    return session.finalizePromise;
  };

  // Always restore a session if this component is removed while an async
  // suspend, update, or resume operation is still in flight.
  useEffect(() => {
    mountedRef.current = true;

    return () => {
      mountedRef.current = false;
      const session = recordingSessionRef.current;
      if (session) void finishRecording(session);
    };
  }, []);

  useEffect(() => {
    // Only add event listeners when we're in editing mode
    if (editingShortcutId === null) return;

    const session = recordingSessionRef.current;
    if (!session || session.id !== editingShortcutId) return;

    // Keyboard event listeners
    const handleKeyDown = (e: KeyboardEvent) => {
      if (!mountedRef.current || recordingSessionRef.current !== session) {
        return;
      }
      if (e.repeat) return; // ignore auto-repeat
      e.preventDefault();

      // Get the key with OS-specific naming and normalize it
      const rawKey = getKeyName(e, osType);
      const key = normalizeKey(rawKey);

      if (!keyPressed.includes(key)) {
        setKeyPressed((prev) => [...prev, key]);
        // Also add to recorded keys if not already there
        if (!recordedKeys.includes(key)) {
          setRecordedKeys((prev) => [...prev, key]);
        }
      }
    };

    const handleKeyUp = (e: KeyboardEvent) => {
      if (!mountedRef.current || recordingSessionRef.current !== session) {
        return;
      }
      e.preventDefault();

      // Get the key with OS-specific naming and normalize it
      const rawKey = getKeyName(e, osType);
      const key = normalizeKey(rawKey);

      // Remove from currently pressed keys
      setKeyPressed((prev) => prev.filter((k) => k !== key));

      // If no keys are pressed anymore, commit the shortcut
      const updatedKeyPressed = keyPressed.filter((k) => k !== key);
      if (updatedKeyPressed.length === 0 && recordedKeys.length > 0) {
        // Create the shortcut string from all recorded keys
        // Sort keys so modifiers come first, then the main key
        const modifiers = [
          "ctrl",
          "control",
          "shift",
          "alt",
          "option",
          "meta",
          "command",
          "cmd",
          "super",
          "win",
          "windows",
        ];
        const sortedKeys = [...recordedKeys].sort((a, b) => {
          const aIsModifier = modifiers.includes(a.toLowerCase());
          const bIsModifier = modifiers.includes(b.toLowerCase());
          if (aIsModifier && !bIsModifier) return -1;
          if (!aIsModifier && bIsModifier) return 1;
          return 0;
        });
        const newShortcut = sortedKeys.join("+");

        void finishRecording(session, newShortcut);
      }
    };

    // Add click outside handler
    const handleClickOutside = (e: MouseEvent) => {
      if (!mountedRef.current || recordingSessionRef.current !== session) {
        return;
      }
      const activeElement = shortcutRefs.current.get(editingShortcutId);
      if (!activeElement || !activeElement.contains(e.target as Node)) {
        void finishRecording(session);
      }
    };

    window.addEventListener("keydown", handleKeyDown);
    window.addEventListener("keyup", handleKeyUp);
    window.addEventListener("click", handleClickOutside);

    return () => {
      window.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("keyup", handleKeyUp);
      window.removeEventListener("click", handleClickOutside);
    };
  }, [keyPressed, recordedKeys, editingShortcutId, osType, finishRecording]);

  // Start recording a new shortcut
  const startRecording = async (id: string) => {
    if (recordingSessionRef.current || editingShortcutId !== null) return;

    const session: RecordingSession = {
      id,
      originalBinding: bindings[id]?.current_binding || "",
      bindingIds: relevantBindingIds,
      suspendPromise: Promise.resolve({
        successfulIds: [],
        errors: [],
      }),
    };
    recordingSessionRef.current = session;

    // Suspend every relevant binding so no shortcut fires (or swallows the
    // keystrokes) while keys are being recorded.
    session.suspendPromise = suspendBindings(session.bindingIds);
    const suspendResult = await session.suspendPromise;

    if (
      suspendResult.errors.length > 0 ||
      !mountedRef.current ||
      recordingSessionRef.current !== session
    ) {
      void finishRecording(session);
      return;
    }

    setEditingShortcutId(id);
    setKeyPressed([]);
    setRecordedKeys([]);
  };

  // Format the current shortcut keys being recorded
  const formatCurrentKeys = (): string => {
    if (recordedKeys.length === 0)
      return t("settings.general.shortcut.pressKeys");

    // Use the same formatting as the display to ensure consistency
    return formatKeyCombination(recordedKeys.join("+"), osType);
  };

  // Store references to shortcut elements
  const setShortcutRef = (id: string, ref: HTMLDivElement | null) => {
    shortcutRefs.current.set(id, ref);
  };

  // If still loading, show loading state
  if (isLoading) {
    return (
      <SettingContainer
        title={t("settings.general.shortcut.title")}
        description={t("settings.general.shortcut.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="text-sm text-mid-gray">
          {t("settings.general.shortcut.loading")}
        </div>
      </SettingContainer>
    );
  }

  // If no bindings are loaded, show empty state
  if (Object.keys(bindings).length === 0) {
    return (
      <SettingContainer
        title={t("settings.general.shortcut.title")}
        description={t("settings.general.shortcut.description")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="text-sm text-mid-gray">
          {t("settings.general.shortcut.none")}
        </div>
      </SettingContainer>
    );
  }

  const binding = bindings[shortcutId];
  if (!binding) {
    return (
      <SettingContainer
        title={t("settings.general.shortcut.title")}
        description={t("settings.general.shortcut.notFound")}
        descriptionMode={descriptionMode}
        grouped={grouped}
      >
        <div className="text-sm text-mid-gray">
          {t("settings.general.shortcut.none")}
        </div>
      </SettingContainer>
    );
  }

  // Get translated name and description for the binding
  const translatedName = t(
    `settings.general.shortcut.bindings.${shortcutId}.name`,
    binding.name,
  );
  const translatedDescription = t(
    `settings.general.shortcut.bindings.${shortcutId}.description`,
    binding.description,
  );

  return (
    <SettingContainer
      title={translatedName}
      description={translatedDescription}
      descriptionMode={descriptionMode}
      grouped={grouped}
      disabled={disabled}
      layout="horizontal"
    >
      <div className="flex items-center space-x-1">
        {editingShortcutId === shortcutId ? (
          <div
            ref={(ref) => setShortcutRef(shortcutId, ref)}
            className="px-2 py-1 text-sm font-semibold border border-logo-primary bg-logo-primary/30 rounded-md"
          >
            {formatCurrentKeys()}
          </div>
        ) : (
          <div
            className="px-2 py-1 text-sm font-semibold bg-mid-gray/10 border border-mid-gray/80 hover:bg-logo-primary/10 rounded-md cursor-pointer hover:border-logo-primary"
            onClick={() => startRecording(shortcutId)}
          >
            {formatKeyCombination(binding.current_binding, osType)}
          </div>
        )}
        <ResetButton
          onClick={() => resetBinding(shortcutId)}
          disabled={isUpdating(`binding_${shortcutId}`)}
        />
      </div>
    </SettingContainer>
  );
};
