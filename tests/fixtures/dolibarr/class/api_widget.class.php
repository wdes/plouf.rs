<?php
// A synthetic Dolibarr REST API endpoint class.
require_once DOL_DOCUMENT_ROOT.'/api/class/api.class.php';

/**
 * API class for widgets
 *
 * @access protected
 * @class  DolibarrApiAccess {@requires user,external}
 */
class Widgets extends DolibarrApi
{
    /**
     * Get a widget by ref
     *
     * @param  string $ref  Reference
     * @url GET ref/{ref}
     */
    public function getByRef($ref)
    {
        return $this->_fetch(0, $ref);
    }

    public function get($id)
    {
        if (!DolibarrApiAccess::$user->hasRight('widgetshop', 'read')) {
            throw new RestException(403);
        }
        return $this->_fetch($id);
    }

    private function _fetch($id, $ref = '')
    {
        return null;
    }
}
