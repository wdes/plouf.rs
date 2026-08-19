const AppPage = () => import('./App.vue');

export const routes = [
    { path: '/app', name: 'app', component: AppPage, meta: { titleKey: 'x' } },
    { path: '/clients', name: 'clients', component: AppPage },
];
