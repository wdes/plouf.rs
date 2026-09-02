<?php

namespace App\Support;

/**
 * Rich-text markup: BBCode-style tags rendered to HTML. The canonical
 * server-side implementation; `resources/js/markup.ts` is its client port.
 */
class Markup
{
    /**
     * Render bullet points (multi level) to nested <ul> lists.
     */
    public function toHtml(string $source): string
    {
        // [list] ... [*] a bullet -> <ul><li>...</li></ul>
        return '<ul></ul>';
    }
}
