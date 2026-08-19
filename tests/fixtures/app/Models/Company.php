<?php

namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class Company extends Model
{
    protected $table = 'companies';

    public function invoices()
    {
        return $this->hasMany(Invoice::class);
    }

    public function owner()
    {
        return $this->belongsTo(User::class);
    }

    public function label(): string
    {
        return __('company.label');
    }
}
