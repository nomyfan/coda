import { memo, useDeferredValue } from "react";
import ReactMarkdown, { type Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { useHighlighted } from "@/lib/shiki";
import { cn } from "@/lib/utils";

/* Highlighting re-runs on every streamed chunk, so it rides a deferred copy of
 * the text: React can drop stale passes and keep the plain-text render (and
 * typing) responsive while a long block is still growing. */
function CodeBlock({ code, className }: { code: string; className?: string }) {
  const lang = /language-([\w+#-]+)/.exec(className ?? "")?.[1];
  const deferred = useDeferredValue(code);
  const html = useHighlighted(deferred, lang);

  if (html === null) {
    return <code className={className}>{code}</code>;
  }
  return (
    <code className={cn(className, "shiki-code")} dangerouslySetInnerHTML={{ __html: html }} />
  );
}

const components: Components = {
  p: ({ children }) => <p className="my-2 first:mt-0 last:mb-0">{children}</p>,
  a: ({ children, href }) => (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      className="font-medium underline underline-offset-2 hover:opacity-80"
    >
      {children}
    </a>
  ),
  ul: ({ children }) => (
    <ul className="my-2 ml-5 list-disc space-y-1 first:mt-0 last:mb-0">{children}</ul>
  ),
  ol: ({ children }) => (
    <ol className="my-2 ml-5 list-decimal space-y-1 first:mt-0 last:mb-0">{children}</ol>
  ),
  li: ({ children }) => <li className="leading-6">{children}</li>,
  h1: ({ children }) => (
    <h1 className="mb-2 mt-4 text-base font-semibold first:mt-0">{children}</h1>
  ),
  h2: ({ children }) => (
    <h2 className="mb-2 mt-4 text-base font-semibold first:mt-0">{children}</h2>
  ),
  h3: ({ children }) => (
    <h3 className="mb-1.5 mt-3 text-sm font-semibold first:mt-0">{children}</h3>
  ),
  blockquote: ({ children }) => (
    <blockquote className="my-2 border-l-2 border-border pl-3 text-muted-foreground first:mt-0 last:mb-0">
      {children}
    </blockquote>
  ),
  hr: () => <hr className="my-3 border-border" />,
  code: ({ className, children }) => {
    const raw = String(children);
    const isBlock = /language-/.test(className ?? "") || raw.includes("\n");
    if (isBlock) {
      // The fence's trailing newline would otherwise render as a blank line.
      return <CodeBlock code={raw.replace(/\n$/, "")} className={className} />;
    }
    return (
      <code className="rounded bg-black/10 px-1 py-0.5 font-mono text-[0.85em] dark:bg-white/10">
        {children}
      </code>
    );
  },
  pre: ({ children }) => (
    <pre className="my-2 overflow-x-auto rounded-md bg-muted p-3 font-mono text-xs leading-5 text-foreground first:mt-0 last:mb-0 [&_code]:rounded-none [&_code]:bg-transparent [&_code]:p-0 [&_code]:text-inherit">
      {children}
    </pre>
  ),
  table: ({ children }) => (
    <div className="my-2 overflow-x-auto first:mt-0 last:mb-0">
      <table className="w-full border-collapse text-sm">{children}</table>
    </div>
  ),
  th: ({ children }) => (
    <th className="border border-border px-2 py-1 text-left font-medium">{children}</th>
  ),
  td: ({ children }) => <td className="border border-border px-2 py-1">{children}</td>,
};

export const Markdown = memo(function Markdown({
  children,
  className,
}: {
  children: string;
  className?: string;
}) {
  return (
    <div className={cn("text-sm leading-6 break-words", className)}>
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={components}>
        {children}
      </ReactMarkdown>
    </div>
  );
});
