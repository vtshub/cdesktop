import { useEffect, useRef } from 'react';
import { useLexicalComposerContext } from '@lexical/react/LexicalComposerContext';

interface ClickableCodePluginProps {
  /** Function to find a matching diff path (supports partial/right-hand match) */
  findMatchingDiffPath: (text: string) => string | null;
  /** Callback when a clickable code element is clicked (receives the full path) */
  onCodeClick: (fullPath: string, event: MouseEvent) => void;
  /** Tooltip for clickable code elements */
  clickTitle?: string;
}

/**
 * Plugin that makes inline code elements clickable when their content
 * matches a file path in the current diffs.
 *
 * Supports fuzzy right-hand matching: "ChatMarkdown.tsx" will match
 * "src/components/ui-new/primitives/conversation/ChatMarkdown.tsx"
 *
 * Only active in read-only mode. Adds hover styling and click handlers
 * to matching code elements.
 */
export function ClickableCodePlugin({
  findMatchingDiffPath,
  onCodeClick,
  clickTitle,
}: ClickableCodePluginProps) {
  const [editor] = useLexicalComposerContext();
  const processedElementsRef = useRef<WeakSet<Element>>(new WeakSet());

  useEffect(() => {
    const root = editor.getRootElement();
    if (!root) return;
    const handlers = new Map<HTMLElement, (e: Event) => void>();

    // Process a single code element
    const processCodeElement = (element: Element) => {
      // Skip if already processed
      if (processedElementsRef.current.has(element)) return;

      const text = element.textContent?.trim() ?? '';

      // Check if this matches a diff path (supports fuzzy right-hand match)
      const matchedPath = findMatchingDiffPath(text);
      if (!matchedPath) return;

      // Mark as processed
      processedElementsRef.current.add(element);

      // Add clickable styling
      (element as HTMLElement).style.cursor = 'pointer';
      element.classList.add('clickable-code');
      if (clickTitle) (element as HTMLElement).title = clickTitle;

      // Add click handler - use the full matched path for navigation
      const handleClick = (e: Event) => {
        e.preventDefault();
        e.stopPropagation();
        onCodeClick(matchedPath, e as MouseEvent);
      };

      element.addEventListener('click', handleClick);
      handlers.set(element as HTMLElement, handleClick);

      // Store cleanup function on the element
      (element as HTMLElement).dataset.clickableCode = 'true';
    };

    // Process all existing code elements
    const processAllCodeElements = () => {
      // Inline code uses the theme class which includes 'font-mono' and 'bg-muted'
      // The actual class applied is from theme.text.code
      const codeElements = root.querySelectorAll(
        'code, .font-mono.bg-muted, [class*="text-code"]'
      );
      codeElements.forEach(processCodeElement);
    };

    // Initial processing
    processAllCodeElements();

    // Watch for new code elements being added
    const observer = new MutationObserver((mutations) => {
      for (const mutation of mutations) {
        // Check added nodes
        for (const node of mutation.addedNodes) {
          if (node instanceof Element) {
            // Check if the node itself is a code element
            if (
              node.matches('code, .font-mono.bg-muted, [class*="text-code"]')
            ) {
              processCodeElement(node);
            }
            // Check child code elements
            const childCodeElements = node.querySelectorAll(
              'code, .font-mono.bg-muted, [class*="text-code"]'
            );
            childCodeElements.forEach(processCodeElement);
          }
        }
      }
    });

    observer.observe(root, {
      childList: true,
      subtree: true,
    });

    return () => {
      observer.disconnect();
      // Clean up click handlers (a re-run would otherwise add a second one)
      handlers.forEach((handler, el) => {
        el.removeEventListener('click', handler);
        el.style.cursor = '';
        el.removeAttribute('title');
        el.classList.remove('clickable-code');
        delete el.dataset.clickableCode;
      });
      processedElementsRef.current = new WeakSet();
    };
  }, [editor, findMatchingDiffPath, onCodeClick, clickTitle]);

  return null;
}
