<?php

namespace App\Support;

use App\Models\Company;

interface Greeter
{
    public function greet(): string;
}

trait Named
{
    public function label(): string
    {
        return 'x';
    }
}

enum Color: string
{
    case Red = 'r';
    case Blue = 'b';
}

function helper(Company $c): int
{
    $c->invoices();
    $fn = function () {
        return 1;
    };
    $arrow = fn () => 2;
    return strlen('x');
}

class Service implements Greeter
{
    use Named;

    public function greet(): string
    {
        return self::make();
    }

    public static function make(): string
    {
        return __('service.hi');
    }

    public function go(Company $c): string
    {
        return $c->label();
    }
}
