<?php

namespace App\Models;

use Illuminate\Database\Eloquent\Model;

class Invoice extends Model
{
    public function company()
    {
        return $this->belongsTo(Company::class);
    }
}
