<template>
    <div>{{ $t('app.title') }}</div>
</template>

<script setup lang="ts">
import { useI18n } from 'vue-i18n';

const { t } = useI18n();
const sub = t('app.sub');
</script>
