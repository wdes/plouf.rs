/**
 * Rich-text markup rendered to HTML -- the client-side port of the PHP
 * `Markup` class. Kept name-for-name so the two are twins.
 */
export class Markup {
    toHtml(source: string): string {
        // bullet points multi level -> nested <ul>
        return '<ul></ul>';
    }
}
