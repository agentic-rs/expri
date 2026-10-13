import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Icon, type IconName } from "./dashboard_icons";

type Position = { left: number; top: number; width: number; max_height: number };

/** A nonmodal panel in a portal stays outside the history's scrolling boundary. */
export function RunControlPopover({ id, label, icon, count = 0, open, on_change, disabled = false, hidden = false, children }: {
  id: string;
  label: string;
  icon: IconName;
  count?: number;
  open: boolean;
  on_change: (open: boolean) => void;
  disabled?: boolean;
  hidden?: boolean;
  children: ReactNode;
}) {
  const trigger = useRef<HTMLElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const on_change_ref = useRef(on_change);
  on_change_ref.current = on_change;
  const [position, setPosition] = useState<Position>({ left: 12, top: 60, width: 360, max_height: 480 });
  function close(restore_focus = false): void {
    on_change_ref.current(false);
    if (restore_focus) trigger.current?.focus({ preventScroll: true });
  }
  useLayoutEffect(() => {
    if (!open || hidden) return;
    const place = () => {
      const anchor = trigger.current?.getBoundingClientRect();
      if (!anchor) return;
      if (anchor.bottom <= 0 || anchor.top >= window.innerHeight) {
        on_change_ref.current(false);
        return;
      }
      const width = Math.min(380, window.innerWidth - 24);
      const below = window.innerHeight - anchor.bottom - 20;
      const above = below < 160 && anchor.top > below;
      const top = above ? 12 : Math.max(12, anchor.bottom + 8);
      const available_height = Math.max(0, Math.min(window.innerHeight - top - 12, above ? anchor.top - 20 : below));
      setPosition({
        width,
        left: Math.max(12, Math.min(anchor.left, window.innerWidth - width - 12)),
        top,
        max_height: Math.min(480, available_height),
      });
    };
    place();
    panel.current?.focus({ preventScroll: true });
    window.addEventListener("resize", place);
    window.addEventListener("scroll", place, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [open, hidden]);
  useEffect(() => {
    if (!open || hidden) return;
    const outside = (event: MouseEvent | FocusEvent) => {
      const target = event.target as Node | null;
      if (!trigger.current?.contains(target) && !panel.current?.contains(target)) close();
    };
    const keyboard = (event: globalThis.KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      event.stopPropagation();
      close(true);
    };
    document.addEventListener("click", outside, true);
    document.addEventListener("focusin", outside);
    document.addEventListener("keydown", keyboard);
    return () => {
      document.removeEventListener("click", outside, true);
      document.removeEventListener("focusin", outside);
      document.removeEventListener("keydown", keyboard);
    };
  }, [open, hidden]);
  return (
    <>
      <details id={id} className="run-control" open={open && !hidden} hidden={hidden}
        onToggle={(event) => {
          if (event.currentTarget.open !== open) on_change(!disabled && event.currentTarget.open);
        }}>
        <summary ref={trigger} id={`${id}-trigger`} className="run-control-trigger"
          aria-controls={`${id}-panel`} aria-expanded={open && !hidden} aria-disabled={disabled}
          aria-label={`${label}${count ? ` (${count})` : ""}`} title={label} tabIndex={disabled ? -1 : 0}
          onClick={(event) => { event.preventDefault(); if (!disabled) on_change(!open); }}>
          <Icon name={icon} /><span className="run-control-label">{label}</span>
          {count > 0 && <span className="run-control-count">{count}</span>}
        </summary>
      </details>
      {createPortal(
        <div ref={panel} id={`${id}-panel`} className="run-popover-panel" role="dialog"
          aria-labelledby={`${id}-trigger`} tabIndex={-1} hidden={!open || hidden}
          style={{ left: position.left, top: position.top, width: position.width, maxHeight: position.max_height }}>
          <div className="run-popover-heading"><h3>{label}</h3>
            <button type="button" className="run-popover-close" aria-label={`Close ${label}`} onClick={() => close(true)}><Icon name="close" /></button>
          </div>
          {children}
        </div>, document.body,
      )}
    </>
  );
}
