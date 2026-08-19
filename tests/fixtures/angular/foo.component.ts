@Component({
    selector: 'app-foo',
    templateUrl: './foo.html',
})
export class FooComponent {
    ngOnInit(): void {
        this.translate.instant('ng.key');
    }
}
