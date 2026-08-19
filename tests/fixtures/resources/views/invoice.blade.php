@extends('layouts.app')
@include('partials.header')
<x-alert class="a" />
<livewire:nav-bar />
@lang('invoice.title')
{{ __('invoice.total') }}
