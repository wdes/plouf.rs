<?php

use Illuminate\Database\Migrations\Migration;

return new class extends Migration {
    public function up(): void
    {
        Schema::create('companies', function ($t) {
            $t->string('name');
        });
        DB::table('users')->count();
    }

    public function down(): void
    {
        Schema::dropIfExists('companies');
    }
};
