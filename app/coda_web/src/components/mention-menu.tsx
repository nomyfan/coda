import { File, Folder } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import {
  BUILTIN_COMMANDS,
  type MentionItem,
  type MentionTrigger,
  mentionName,
  mentionParent,
  rankMentionItems,
} from "@/lib/composer-mentions";
import { fetchWorkspaceFiles, fetchWorkspaceSkills } from "@/store/session";

/** How long typing has to pause before an `@` query goes to the server. Long
 * enough that a burst of keystrokes is one request, short enough that the menu
 * still feels like it is following along. */
const FILE_SEARCH_DEBOUNCE_MS = 120;

export type MentionResults = {
  items: MentionItem[];
  loading: boolean;
  /** Why the menu has nothing to show, when that is a failure rather than an
   * honest empty result. */
  error: string | null;
  /** Matches were left out; typing more of the query is what narrows them. */
  truncated: boolean;
};

const NO_RESULTS: MentionResults = {
  items: [],
  loading: false,
  error: null,
  truncated: false,
};

/**
 * The items a trigger offers.
 *
 * The two menus are fed differently on purpose. Files are searched server-side
 * per keystroke (debounced): a workspace holds more paths than are worth
 * shipping to the browser, and the server already ranks them. Skills are a
 * handful of entries, so they are fetched once when the menu opens and filtered
 * locally — no round trip per character.
 */
export function useMentionItems({
  trigger,
  serverUrl,
  workspaceId,
  enabled,
}: {
  trigger: MentionTrigger | null;
  serverUrl: string;
  workspaceId: string;
  enabled: boolean;
}): MentionResults {
  const [files, setFiles] = useState<MentionResults>(NO_RESULTS);
  const [skills, setSkills] = useState<MentionResults>(NO_RESULTS);

  const active = enabled && Boolean(serverUrl) && Boolean(workspaceId) ? trigger : null;
  const fileQuery = active?.kind === "file" ? active.query : null;
  // The `/` menu is fetched once per opening, not per keystroke, so the effect
  // must not see the query at all — only whether a menu is open.
  const slashOpen = active?.kind === "slash";

  useEffect(() => {
    if (fileQuery === null) {
      setFiles(NO_RESULTS);
      return;
    }
    let live = true;
    setFiles((previous) => ({ ...previous, loading: true, error: null }));
    const timer = setTimeout(() => {
      fetchWorkspaceFiles(serverUrl, workspaceId, fileQuery)
        .then((catalog) => {
          if (!live) return;
          setFiles({
            items: catalog.files.map((file) => ({
              kind: file.is_dir ? "directory" : "file",
              value: file.path,
            })),
            loading: false,
            error: null,
            truncated: catalog.truncated,
          });
        })
        .catch((err: unknown) => {
          if (!live) return;
          setFiles({
            items: [],
            loading: false,
            error: err instanceof Error ? err.message : "Could not search this workspace",
            truncated: false,
          });
        });
    }, FILE_SEARCH_DEBOUNCE_MS);

    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [fileQuery, serverUrl, workspaceId]);

  useEffect(() => {
    if (!slashOpen) {
      setSkills(NO_RESULTS);
      return;
    }
    let live = true;
    setSkills({ items: [], loading: true, error: null, truncated: false });
    fetchWorkspaceSkills(serverUrl, workspaceId)
      .then((found) => {
        if (!live) return;
        setSkills({
          items: found.map((skill) => ({
            kind: "skill",
            value: skill.name,
            detail: skill.description,
          })),
          loading: false,
          error: null,
          truncated: false,
        });
      })
      .catch((err: unknown) => {
        if (!live) return;
        setSkills({
          items: [],
          loading: false,
          error: err instanceof Error ? err.message : "Could not read this workspace's skills",
          truncated: false,
        });
      });

    return () => {
      live = false;
    };
  }, [slashOpen, serverUrl, workspaceId]);

  if (active === null) {
    return NO_RESULTS;
  }
  if (active.kind === "file") {
    return files;
  }
  // Commands are the whole message or nothing, so they only join the list when
  // the `/` opens it. Skills can be named anywhere, so they are always there.
  const commands: MentionItem[] = active.atMessageStart
    ? BUILTIN_COMMANDS.map((command) => ({
        kind: "command",
        value: command.name,
        detail: command.description,
      }))
    : [];
  return {
    ...skills,
    items: rankMentionItems([...commands, ...skills.items], active.query),
  };
}

const ITEM_ICONS = {
  file: File,
  directory: Folder,
  skill: undefined,
  command: undefined,
} as const;

/**
 * The popup above the composer. Purely presentational: the composer owns the
 * trigger, the active index, and what accepting an item does — the textarea
 * keeps focus and the keyboard throughout, which is why rows commit on
 * `mousedown` rather than taking focus of their own.
 */
export function MentionMenu({
  results,
  activeIndex,
  listId,
  optionId,
  emptyLabel,
  onSelect,
  onHover,
}: {
  results: MentionResults;
  activeIndex: number;
  listId: string;
  optionId: (index: number) => string;
  /** What "nothing matched" reads as for this trigger. */
  emptyLabel: string;
  onSelect: (item: MentionItem) => void;
  onHover: (index: number) => void;
}) {
  const activeRef = useRef<HTMLLIElement>(null);

  useEffect(() => {
    activeRef.current?.scrollIntoView({ block: "nearest" });
  }, [activeIndex]);

  const { items, loading, error, truncated } = results;

  return (
    <div className="absolute bottom-full left-0 right-0 z-20 mb-1.5 overflow-hidden rounded-md border border-border bg-popover text-popover-foreground shadow-lg">
      <ul id={listId} role="listbox" className="max-h-56 overflow-y-auto py-1">
        {items.map((item, index) => {
          const Icon = ITEM_ICONS[item.kind];
          const isFile = item.kind === "file" || item.kind === "directory";
          const name = isFile ? mentionName(item.value) : `/${item.value}`;
          const detail = isFile ? mentionParent(item.value) : item.detail;
          return (
            <li
              key={`${item.kind}:${item.value}`}
              ref={index === activeIndex ? activeRef : undefined}
              id={optionId(index)}
              role="option"
              aria-selected={index === activeIndex}
              className={[
                "flex cursor-pointer items-center gap-2 px-2.5 py-1.5 text-sm",
                index === activeIndex ? "bg-accent text-accent-foreground" : "",
              ]
                .filter(Boolean)
                .join(" ")}
              // Committing on mousedown keeps the caret where it is: a click
              // that first blurred the textarea would lose the insertion point.
              onMouseDown={(event) => {
                event.preventDefault();
                onSelect(item);
              }}
              onMouseEnter={() => onHover(index)}
            >
              {Icon ? <Icon className="size-3.5 shrink-0 text-muted-foreground" /> : null}
              <span className="shrink-0 truncate font-medium">
                {name}
                {item.kind === "directory" ? "/" : ""}
              </span>
              {detail ? (
                <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
                  {detail}
                </span>
              ) : null}
            </li>
          );
        })}
        {items.length === 0 ? (
          <li
            className={[
              "px-2.5 py-1.5 text-sm",
              error ? "text-destructive" : "text-muted-foreground",
            ].join(" ")}
          >
            {error ?? (loading ? "Searching…" : emptyLabel)}
          </li>
        ) : null}
      </ul>
      <div className="flex items-center justify-between gap-2 border-t border-border px-2.5 py-1 text-[11px] text-muted-foreground">
        <span>↑↓ to move · Enter to insert · Esc to dismiss</span>
        {truncated ? <span>more matches — keep typing</span> : null}
      </div>
    </div>
  );
}
