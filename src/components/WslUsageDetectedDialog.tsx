import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { useQueryClient } from "@tanstack/react-query";
import { Terminal } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { useSettingsQuery } from "@/lib/query";
import { settingsApi, usageApi } from "@/lib/api";
import type { WslToolDetection } from "@/types/usage";

/**
 * WSL 用量同步的一次性询问。
 *
 * 仅当 `wslUsagePromptConfirmed` 为空（后端在非 Windows 上启动时会直接写
 * true）且真的在 WSL 里探测到工具时才弹出——探测不到就没必要打扰用户。
 */
export function WslUsageDetectedDialog() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data: settings } = useSettingsQuery();
  const [detections, setDetections] = useState<WslToolDetection[] | null>(null);

  const needsPrompt =
    settings != null && settings.wslUsagePromptConfirmed !== true;

  useEffect(() => {
    if (!needsPrompt || detections !== null) return;
    let cancelled = false;

    void usageApi
      .detectWslSources()
      .then((result) => {
        if (!cancelled) setDetections(result);
      })
      .catch((error) => {
        // 探测失败当作"没有 WSL"处理：宁可漏弹也不要因为故障打扰用户
        console.error("Failed to detect WSL usage sources:", error);
        if (!cancelled) setDetections([]);
      });

    return () => {
      cancelled = true;
    };
  }, [needsPrompt, detections]);

  const isOpen = needsPrompt && detections != null && detections.length > 0;

  const persist = async (enable: boolean) => {
    if (!settings) return;
    try {
      const { webdavSync: _, ...rest } = settings;
      await settingsApi.save({
        ...rest,
        enableWslUsageSync: enable,
        wslUsagePromptConfirmed: true,
      });
      await queryClient.invalidateQueries({ queryKey: ["settings"] });
      if (enable) {
        // 立刻跑一次同步，让用户马上能在统计页看到 WSL 里的数据
        await usageApi.syncSessionUsage();
        await queryClient.invalidateQueries({ queryKey: ["usage"] });
      }
    } catch (error) {
      console.error("Failed to save WSL usage sync preference:", error);
    }
  };

  return (
    <Dialog
      open={isOpen}
      onOpenChange={(open) => {
        // 关闭等同于"暂不开启"，仍然记下已询问过，避免每次启动重复打扰
        if (!open) void persist(false);
      }}
    >
      <DialogContent className="max-w-md" zIndex="top">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Terminal className="h-5 w-5 text-blue-500" />
            {t("wslUsageNotice.title")}
          </DialogTitle>
        </DialogHeader>
        <div className="space-y-3 px-6 py-5">
          <DialogDescription className="whitespace-pre-line leading-relaxed">
            {t("wslUsageNotice.body")}
          </DialogDescription>
          <ul className="space-y-1 text-sm">
            {(detections ?? []).map((item) => (
              <li key={item.distro} className="flex gap-2">
                <span className="font-medium">{item.distro}</span>
                <span className="text-muted-foreground">
                  {item.tools.join("、")}
                </span>
              </li>
            ))}
          </ul>
          <DialogDescription className="leading-relaxed">
            {t("wslUsageNotice.hint")}
          </DialogDescription>
        </div>
        <DialogFooter>
          <Button variant="outline" onClick={() => void persist(false)}>
            {t("wslUsageNotice.decline")}
          </Button>
          <Button onClick={() => void persist(true)}>
            {t("wslUsageNotice.confirm")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
