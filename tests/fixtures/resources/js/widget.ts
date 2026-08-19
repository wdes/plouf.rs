import { Foo } from './foo';

export interface Shape {
    area(): number;
}

export enum Size {
    S,
    M,
}

export class Widget extends Base implements Shape {
    area(): number {
        return this.render();
    }
    render(): number {
        return t('nav.home') + Foo;
    }
}

export function make(): Widget {
    const w = new Widget();
    return w;
}
